use crate::{codex, mcp, setup};
use distill::{
    ArtifactRef, BinaryPolicy, Budget, ByteString, CL100K_PROFILE, CONTRACT_VERSION, CountUnit,
    Engine, EngineConfig, Failure, FailureCode, Outcome, Request, Retention, ScalarValue, Source,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    io::{self, Read, Write},
    path::PathBuf,
};

const CLI_SCHEMA_VERSION: &str = "distill.cli/v1";
const BROKEN_PIPE_EXIT: i32 = 74;

const HELP: &str = r#"distill: local bounded context projection

Usage:
  distill [--store PATH] [--root ID=PATH] project --budget N [--json]
  distill [--store PATH] artifact get ID [--json]
  distill [--store PATH] artifact trace ID [--json]
  distill [--store PATH] status [--json]
  distill [--store PATH] gc [--json]
  distill [--store PATH] [--root ID=PATH] read --root-id ID --path PATH --budget N [--json]
  distill [--store PATH] [--root ID=PATH] run --cwd-root ID --cwd PATH --budget N [--timeout MS] -- EXECUTABLE [ARG...]
  distill [--store PATH] codex-hook --mode off|observe|active
  distill [--store PATH] [--root ID=PATH] mcp
  distill setup codex|claude --config PATH --command PATH [--dry-run|--restore]

Budget options:
  --budget N          Total visible limit (required)
  --reserve N         Adapter envelope allowance (default: 0)
  --unit bytes|tokens Count unit (default: bytes)
  --profile NAME      Central preservation profile (default: plain-text/v1)
  --ttl SECONDS       Artifact retention from capture time

Stable exit codes:
  0 success
  2 invalid input or unsupported schema
  3 impossible budget
  4 unavailable, expired, or corrupt artifact
  5 artifact-store failure
  6 acquisition failure
  70 internal invariant breach
  74 output consumer closed the pipe

All commands are non-interactive. JSON mode writes only versioned protocol
objects to stdout. Diagnostics use stderr. NO_COLOR is honored by never emitting
color. Codex hooks cover supported local tools only: hosted tools and specialized
paths that bypass PostToolUse cannot be projected. Claude projection is explicit
through distill_read and distill_run; native Read and Bash are not intercepted.
"#;

#[derive(Clone, Debug)]
struct GlobalOptions {
    store_path: PathBuf,
    roots: BTreeMap<String, PathBuf>,
}

#[derive(Clone, Debug)]
struct ProjectionOptions {
    budget: Budget,
    profile: String,
    retention: Retention,
    json: bool,
}

#[derive(Debug)]
pub(crate) struct SurfaceError {
    exit_code: i32,
    code: String,
    message: String,
    artifact: Option<ArtifactRef>,
}

impl SurfaceError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            exit_code: 2,
            code: "invalid_input".to_owned(),
            message: message.into(),
            artifact: None,
        }
    }

    pub(crate) fn output(error: io::Error) -> Self {
        let broken_pipe = error.kind() == io::ErrorKind::BrokenPipe;
        Self {
            exit_code: if broken_pipe { BROKEN_PIPE_EXIT } else { 70 },
            code: if broken_pipe {
                "broken_pipe".to_owned()
            } else {
                "output_failure".to_owned()
            },
            message: if broken_pipe {
                "output consumer closed the pipe".to_owned()
            } else {
                "cannot write command output".to_owned()
            },
            artifact: None,
        }
    }
}

impl From<Failure> for SurfaceError {
    fn from(failure: Failure) -> Self {
        let exit_code = match failure.code {
            FailureCode::InvalidRequest
            | FailureCode::SchemaUnsupported
            | FailureCode::SourceUnsupported
            | FailureCode::TokenProfileUnsupported
            | FailureCode::InputTooLarge
            | FailureCode::ResourceExhausted
            | FailureCode::UnsafeRoot => 2,
            FailureCode::BudgetUnsatisfiable => 3,
            FailureCode::ArtifactUnknown
            | FailureCode::ArtifactExpired
            | FailureCode::ArtifactCorrupt
            | FailureCode::ArtifactSchemaUnsupported => 4,
            FailureCode::PermissionDenied
            | FailureCode::StoreFull
            | FailureCode::StoreBusy
            | FailureCode::CommitFailed => 5,
            FailureCode::AcquisitionFailed => 6,
            FailureCode::InvariantBreach => 70,
        };
        Self {
            exit_code,
            code: failure.code.as_str().to_owned(),
            message: failure.safe_message,
            artifact: failure.artifact,
        }
    }
}

pub(crate) fn run<R: Read, W: Write, E: Write>(
    raw_args: Vec<OsString>,
    mut input: R,
    mut output: W,
    mut diagnostics: E,
) -> i32 {
    let json_mode = raw_args.iter().any(|argument| argument == "--json");
    let result = run_inner(raw_args, &mut input, &mut output, &mut diagnostics);
    match result {
        Ok(()) => 0,
        Err(error) => {
            if error.exit_code == BROKEN_PIPE_EXIT {
                return BROKEN_PIPE_EXIT;
            }
            if json_mode {
                let body = json!({
                    "schema_version": CLI_SCHEMA_VERSION,
                    "ok": false,
                    "error": {
                        "code": error.code,
                        "message": error.message,
                        "artifact": error.artifact,
                    }
                });
                if write_json_line(&mut output, &body).is_err() {
                    return BROKEN_PIPE_EXIT;
                }
            } else {
                let write_result = if let Some(artifact) = &error.artifact {
                    writeln!(
                        diagnostics,
                        "{}: {} artifact={}",
                        error.code, error.message, artifact.id
                    )
                } else {
                    writeln!(diagnostics, "{}: {}", error.code, error.message)
                };
                if write_result.is_err() {
                    return BROKEN_PIPE_EXIT;
                }
            }
            error.exit_code
        }
    }
}

fn run_inner<R: Read, W: Write, E: Write>(
    raw_args: Vec<OsString>,
    input: &mut R,
    output: &mut W,
    diagnostics: &mut E,
) -> Result<(), SurfaceError> {
    let mut args = decode_args(raw_args)?;
    if args.is_empty()
        || matches!(
            args.front().map(String::as_str),
            Some("-h" | "--help" | "help")
        )
    {
        output
            .write_all(HELP.as_bytes())
            .map_err(SurfaceError::output)?;
        return Ok(());
    }
    if args.front().is_some_and(|argument| argument == "setup") {
        args.pop_front();
        return setup::run(args, output);
    }
    let global = parse_global(&mut args)?;
    let command = args
        .pop_front()
        .ok_or_else(|| SurfaceError::invalid("missing command"))?;
    match command.as_str() {
        "project" => project(global, args, input, output, diagnostics),
        "artifact" => artifact(global, args, output),
        "status" => status(global, args, output),
        "gc" => gc(global, args, output),
        "read" => read_file(global, args, output, diagnostics),
        "run" => run_process(global, args, output, diagnostics),
        "codex-hook" => codex::run(global.config(), args, input, output),
        "mcp" => mcp::run(global.config(), input, output, diagnostics),
        _ => Err(SurfaceError::invalid(format!(
            "unknown command '{command}'"
        ))),
    }
}

impl GlobalOptions {
    fn config(&self) -> EngineConfig {
        let mut config = EngineConfig::local(self.store_path.clone());
        config.roots = self.roots.clone();
        config
    }
}

fn decode_args(raw: Vec<OsString>) -> Result<VecDeque<String>, SurfaceError> {
    raw.into_iter()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| SurfaceError::invalid("arguments must be valid UTF-8"))
        })
        .collect()
}

fn default_store_path() -> Result<PathBuf, SurfaceError> {
    if let Some(value) = std::env::var_os("DISTILL_STORE_PATH") {
        return Ok(PathBuf::from(value));
    }
    if let Some(value) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(value).join("distill/artifacts.db"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".local/share/distill/artifacts.db"))
        .ok_or_else(|| {
            SurfaceError::invalid("set --store, DISTILL_STORE_PATH, XDG_DATA_HOME, or HOME")
        })
}

fn parse_global(args: &mut VecDeque<String>) -> Result<GlobalOptions, SurfaceError> {
    let mut store_path = None;
    let mut roots = BTreeMap::new();
    loop {
        match args.front().map(String::as_str) {
            Some("--store") => {
                args.pop_front();
                store_path = Some(PathBuf::from(take(args, "--store")?));
            }
            Some("--root") => {
                args.pop_front();
                let value = take(args, "--root")?;
                let (id, path) = value
                    .split_once('=')
                    .ok_or_else(|| SurfaceError::invalid("--root requires ID=PATH"))?;
                if id.is_empty() || roots.insert(id.to_owned(), PathBuf::from(path)).is_some() {
                    return Err(SurfaceError::invalid(
                        "root IDs must be nonempty and unique",
                    ));
                }
            }
            _ => break,
        }
    }
    Ok(GlobalOptions {
        store_path: match store_path {
            Some(path) => path,
            None => default_store_path()?,
        },
        roots,
    })
}

fn parse_projection(args: &mut VecDeque<String>) -> Result<ProjectionOptions, SurfaceError> {
    let mut total = None;
    let mut reserve = 0_u64;
    let mut unit = CountUnit::Bytes;
    let mut profile = "plain-text/v1".to_owned();
    let mut ttl_seconds = None;
    let mut json = false;
    while let Some(argument) = args.front().cloned() {
        match argument.as_str() {
            "--budget" => {
                args.pop_front();
                total = Some(parse_u64(&take(args, "--budget")?, "--budget")?);
            }
            "--reserve" => {
                args.pop_front();
                reserve = parse_u64(&take(args, "--reserve")?, "--reserve")?;
            }
            "--unit" => {
                args.pop_front();
                unit = match take(args, "--unit")?.as_str() {
                    "bytes" => CountUnit::Bytes,
                    "tokens" => CountUnit::Tokens,
                    _ => {
                        return Err(SurfaceError::invalid("--unit must be bytes or tokens"));
                    }
                };
            }
            "--profile" => {
                args.pop_front();
                profile = take(args, "--profile")?;
            }
            "--ttl" => {
                args.pop_front();
                ttl_seconds = Some(parse_u64(&take(args, "--ttl")?, "--ttl")?);
            }
            "--json" => {
                args.pop_front();
                json = true;
            }
            _ => break,
        }
    }
    let total_visible_limit = total.ok_or_else(|| SurfaceError::invalid("--budget is required"))?;
    Ok(ProjectionOptions {
        budget: Budget {
            unit,
            total_visible_limit,
            reserved_envelope: reserve,
            token_profile: (unit == CountUnit::Tokens).then(|| CL100K_PROFILE.to_owned()),
        },
        profile,
        retention: Retention {
            expires_at: None,
            ttl_seconds,
        },
        json,
    })
}

fn project<R: Read, W: Write, E: Write>(
    global: GlobalOptions,
    mut args: VecDeque<String>,
    input: &mut R,
    output: &mut W,
    diagnostics: &mut E,
) -> Result<(), SurfaceError> {
    let options = parse_projection(&mut args)?;
    ensure_empty(&args)?;
    let bytes = read_bounded(input, 10 * 1024 * 1024)?;
    let json = options.json;
    let outcome = Engine::new(global.config())?.handle(request(
        "cli-project",
        Source::Inline {
            bytes: bytes.into(),
            media_type: Some("application/octet-stream".to_owned()),
        },
        options,
    ))?;
    write_outcome(output, diagnostics, &outcome, json)
}

fn read_file<W: Write, E: Write>(
    global: GlobalOptions,
    mut args: VecDeque<String>,
    output: &mut W,
    diagnostics: &mut E,
) -> Result<(), SurfaceError> {
    let mut root_id = None;
    let mut path = None;
    let mut syntax_hint = None;
    let mut binary_policy = BinaryPolicy::Accept;
    let mut projection_args = VecDeque::new();
    while let Some(argument) = args.pop_front() {
        match argument.as_str() {
            "--root-id" => root_id = Some(take(&mut args, "--root-id")?),
            "--path" => path = Some(take(&mut args, "--path")?),
            "--syntax" => syntax_hint = Some(take(&mut args, "--syntax")?),
            "--reject-binary" => binary_policy = BinaryPolicy::Reject,
            _ => {
                projection_args.push_back(argument);
                projection_args.extend(args);
                break;
            }
        }
    }
    let options = parse_projection(&mut projection_args)?;
    ensure_empty(&projection_args)?;
    let root_id = root_id.ok_or_else(|| SurfaceError::invalid("--root-id is required"))?;
    if !global.roots.contains_key(&root_id) {
        return Err(SurfaceError::invalid(
            "--root-id must name a configured --root",
        ));
    }
    let path = path.ok_or_else(|| SurfaceError::invalid("--path is required"))?;
    let mut request = request(
        "cli-read",
        Source::File {
            root_id,
            relative_path: ByteString::from_utf8(path),
            binary_policy,
        },
        options.clone(),
    );
    if let Some(hint) = syntax_hint {
        request
            .metadata
            .insert("content_class".to_owned(), ScalarValue::String(hint));
    }
    let outcome = Engine::new(global.config())?.handle(request)?;
    write_outcome(output, diagnostics, &outcome, options.json)
}

fn run_process<W: Write, E: Write>(
    global: GlobalOptions,
    mut args: VecDeque<String>,
    output: &mut W,
    diagnostics: &mut E,
) -> Result<(), SurfaceError> {
    let mut cwd_root = None;
    let mut cwd = None;
    let mut timeout_ms = None;
    let mut environment_profile = None;
    let mut projection_args = VecDeque::new();
    let mut executable = None;
    let mut argv = Vec::new();
    while let Some(argument) = args.pop_front() {
        match argument.as_str() {
            "--cwd-root" => cwd_root = Some(take(&mut args, "--cwd-root")?),
            "--cwd" => cwd = Some(take(&mut args, "--cwd")?),
            "--timeout" => {
                timeout_ms = Some(parse_u64(&take(&mut args, "--timeout")?, "--timeout")?)
            }
            "--environment" => environment_profile = Some(take(&mut args, "--environment")?),
            "--" => {
                executable = args.pop_front();
                argv.extend(args);
                break;
            }
            _ => projection_args.push_back(argument),
        }
    }
    let options = parse_projection(&mut projection_args)?;
    ensure_empty(&projection_args)?;
    let cwd_root_id = cwd_root.ok_or_else(|| SurfaceError::invalid("--cwd-root is required"))?;
    if !global.roots.contains_key(&cwd_root_id) {
        return Err(SurfaceError::invalid(
            "--cwd-root must name a configured --root",
        ));
    }
    let source = Source::Process {
        executable: ByteString::from_utf8(
            executable.ok_or_else(|| SurfaceError::invalid("missing executable after --"))?,
        ),
        argv: argv.into_iter().map(ByteString::from_utf8).collect(),
        cwd_root_id,
        cwd_relative_path: ByteString::from_utf8(normalize_root_relative(
            cwd.ok_or_else(|| SurfaceError::invalid("--cwd is required"))?,
        )),
        timeout_ms,
        environment_profile,
    };
    let outcome =
        Engine::new(global.config())?.handle(request("cli-run", source, options.clone()))?;
    write_outcome(output, diagnostics, &outcome, options.json)
}

fn artifact<W: Write>(
    global: GlobalOptions,
    mut args: VecDeque<String>,
    output: &mut W,
) -> Result<(), SurfaceError> {
    let action = args
        .pop_front()
        .ok_or_else(|| SurfaceError::invalid("artifact requires get or trace"))?;
    let target = args
        .pop_front()
        .ok_or_else(|| SurfaceError::invalid("artifact requires an ID or JSON reference"))?;
    let json = remove_flag(&mut args, "--json");
    ensure_empty(&args)?;
    let engine = Engine::new(global.config())?;
    let reference = parse_artifact_target(&engine, &target)?;
    match action.as_str() {
        "get" => {
            let restored = engine.restore(&reference)?;
            if json {
                write_success(output, &restored)
            } else {
                output
                    .write_all(&restored.bytes.0)
                    .map_err(SurfaceError::output)
            }
        }
        "trace" => {
            let trace = engine.trace(&reference)?;
            write_success(output, &trace)
        }
        _ => Err(SurfaceError::invalid("artifact requires get or trace")),
    }
}

fn status<W: Write>(
    global: GlobalOptions,
    mut args: VecDeque<String>,
    output: &mut W,
) -> Result<(), SurfaceError> {
    let _json = remove_flag(&mut args, "--json");
    ensure_empty(&args)?;
    write_success(output, &Engine::new(global.config())?.status()?)
}

fn gc<W: Write>(
    global: GlobalOptions,
    mut args: VecDeque<String>,
    output: &mut W,
) -> Result<(), SurfaceError> {
    let _json = remove_flag(&mut args, "--json");
    ensure_empty(&args)?;
    write_success(output, &Engine::new(global.config())?.collect_garbage()?)
}

fn request(id: &str, source: Source, options: ProjectionOptions) -> Request {
    Request {
        contract_version: CONTRACT_VERSION.to_owned(),
        request_id: id.to_owned(),
        source,
        budget: options.budget,
        preservation_profile: options.profile,
        retention: options.retention,
        metadata: BTreeMap::from([(
            "adapter".to_owned(),
            ScalarValue::String("cli/v1".to_owned()),
        )]),
    }
}

fn write_outcome<W: Write, E: Write>(
    output: &mut W,
    diagnostics: &mut E,
    outcome: &Outcome,
    json: bool,
) -> Result<(), SurfaceError> {
    if json {
        return write_success(output, outcome);
    }
    output
        .write_all(outcome.visible.bytes.as_bytes())
        .map_err(SurfaceError::output)?;
    writeln!(
        diagnostics,
        "artifact={} fidelity={:?} visible={}/{}",
        outcome.artifact.id,
        outcome.receipt.fidelity,
        outcome.receipt.visible_count,
        outcome.receipt.original_count
    )
    .map_err(SurfaceError::output)
}

fn parse_artifact_target(engine: &Engine, target: &str) -> Result<ArtifactRef, SurfaceError> {
    if target.starts_with('{') {
        serde_json::from_str(target)
            .map_err(|_| SurfaceError::invalid("artifact reference JSON is malformed"))
    } else {
        engine.resolve_artifact(target).map_err(Into::into)
    }
}

fn write_success<W: Write, T: Serialize>(output: &mut W, result: &T) -> Result<(), SurfaceError> {
    write_json_line(
        output,
        &json!({
            "schema_version": CLI_SCHEMA_VERSION,
            "ok": true,
            "result": result,
        }),
    )
}

pub(crate) fn write_json_line<W: Write>(output: &mut W, value: &Value) -> Result<(), SurfaceError> {
    serde_json::to_writer(&mut *output, value)
        .map_err(|error| SurfaceError::output(io::Error::other(error)))?;
    output.write_all(b"\n").map_err(SurfaceError::output)
}

fn read_bounded<R: Read>(input: &mut R, limit: usize) -> Result<Vec<u8>, SurfaceError> {
    let mut bytes = Vec::new();
    input
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| SurfaceError::invalid("cannot read stdin"))?;
    if bytes.len() > limit {
        return Err(SurfaceError::from(Failure {
            code: FailureCode::InputTooLarge,
            safe_message: "stdin exceeds the 10 MiB limit".to_owned(),
            request_id: None,
            details: BTreeMap::new(),
            artifact: None,
            acquisition: None,
        }));
    }
    Ok(bytes)
}

fn take(args: &mut VecDeque<String>, flag: &str) -> Result<String, SurfaceError> {
    args.pop_front()
        .ok_or_else(|| SurfaceError::invalid(format!("{flag} requires a value")))
}

fn parse_u64(value: &str, flag: &str) -> Result<u64, SurfaceError> {
    value
        .parse()
        .map_err(|_| SurfaceError::invalid(format!("{flag} requires an unsigned integer")))
}

fn normalize_root_relative(path: String) -> String {
    if path == "." { String::new() } else { path }
}

fn remove_flag(args: &mut VecDeque<String>, flag: &str) -> bool {
    if let Some(position) = args.iter().position(|argument| argument == flag) {
        args.remove(position);
        true
    } else {
        false
    }
}

fn ensure_empty(args: &VecDeque<String>) -> Result<(), SurfaceError> {
    if let Some(argument) = args.front() {
        Err(SurfaceError::invalid(format!(
            "unexpected argument '{argument}'"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn store(temp: &TempDir) -> PathBuf {
        temp.path().join("store/artifacts.db")
    }

    fn run_args(arguments: &[&str], stdin: &[u8]) -> (i32, Vec<u8>, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            arguments.iter().map(OsString::from).collect(),
            stdin,
            &mut stdout,
            &mut stderr,
        );
        (code, stdout, String::from_utf8(stderr).expect("stderr"))
    }

    #[test]
    fn project_restore_trace_status_and_gc_are_versioned() {
        let temp = TempDir::new().expect("temp");
        let store = store(&temp);
        let store_text = store.to_string_lossy();
        let (code, output, stderr) = run_args(
            &["--store", &store_text, "project", "--budget", "8", "--json"],
            b"first\nmiddle\nlast\n",
        );
        assert_eq!(code, 0, "{stderr}");
        let value: Value = serde_json::from_slice(&output).expect("projection JSON");
        assert_eq!(value["schema_version"], CLI_SCHEMA_VERSION);
        let id = value["result"]["artifact"]["id"]
            .as_str()
            .expect("artifact ID");

        for action in ["trace", "get"] {
            let (code, body, error) = run_args(
                &["--store", &store_text, "artifact", action, id, "--json"],
                b"",
            );
            assert_eq!(code, 0, "{error}");
            let body: Value = serde_json::from_slice(&body).expect("artifact JSON");
            assert_eq!(body["ok"], true);
            if action == "trace" {
                assert_eq!(
                    body["result"]["receipts"]
                        .as_array()
                        .expect("receipt lineage")
                        .len(),
                    1
                );
            }
        }
        for command in ["status", "gc"] {
            let (code, body, error) = run_args(&["--store", &store_text, command, "--json"], b"");
            assert_eq!(code, 0, "{error}");
            assert_eq!(
                serde_json::from_slice::<Value>(&body).expect("status JSON")["ok"],
                true
            );
        }
    }

    #[test]
    fn read_and_run_use_configured_roots_without_shell_parsing() {
        let temp = TempDir::new().expect("temp");
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        fs::write(workspace.join("sample.txt"), b"safe file").expect("file");
        let store = store(&temp);
        let store_text = store.to_string_lossy();
        let root = format!("workspace={}", workspace.display());

        let (read_code, read_output, read_error) = run_args(
            &[
                "--store",
                &store_text,
                "--root",
                &root,
                "read",
                "--root-id",
                "workspace",
                "--path",
                "sample.txt",
                "--budget",
                "64",
                "--json",
            ],
            b"",
        );
        assert_eq!(read_code, 0, "{read_error}");
        assert_eq!(
            serde_json::from_slice::<Value>(&read_output).expect("read JSON")["ok"],
            true
        );

        let (run_code, run_output, run_error) = run_args(
            &[
                "--store",
                &store_text,
                "--root",
                &root,
                "run",
                "--cwd-root",
                "workspace",
                "--cwd",
                ".",
                "--budget",
                "64",
                "--",
                "/usr/bin/printf",
                "%s",
                "$(not-a-shell)",
            ],
            b"",
        );
        assert_eq!(run_code, 0, "{run_error}");
        assert_eq!(
            String::from_utf8(run_output).expect("process output"),
            "$(not-a-shell)"
        );
    }

    #[test]
    fn errors_are_safe_versioned_and_stable() {
        let temp = TempDir::new().expect("temp");
        let store = store(&temp);
        let store_text = store.to_string_lossy();
        let (code, output, stderr) = run_args(
            &["--store", &store_text, "project", "--budget", "0", "--json"],
            b"not empty",
        );
        assert_eq!(code, 3);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&output).expect("error JSON");
        assert_eq!(value["error"]["code"], "budget_unsatisfiable");
        assert!(value["error"]["artifact"].is_object());

        let (code, output, stderr) = run_args(
            &["--store", &store_text, "project", "--budget", "0"],
            b"not empty",
        );
        assert_eq!(code, 3);
        assert!(output.is_empty());
        assert!(stderr.contains("artifact="));

        let (code, output, stderr) =
            run_args(&["--store", &store_text, "artifact", "get", "bad"], b"");
        assert_eq!(code, 2);
        assert!(output.is_empty());
        assert!(!stderr.contains("stack"));

        let (code, output, _) = run_args(&["--json", "unknown"], b"");
        assert_eq!(code, 2);
        assert_eq!(
            serde_json::from_slice::<Value>(&output).expect("invalid JSON")["ok"],
            false
        );
    }

    #[test]
    fn empty_input_help_and_argument_validation_are_noninteractive() {
        let temp = TempDir::new().expect("temp");
        let store = store(&temp);
        let store_text = store.to_string_lossy();
        let (code, output, _) = run_args(&[], b"");
        assert_eq!(code, 0);
        assert!(
            String::from_utf8(output)
                .expect("help")
                .contains("non-interactive")
        );

        let (code, output, error) = run_args(
            &["--store", &store_text, "project", "--budget", "1", "--json"],
            b"",
        );
        assert_eq!(code, 0, "{error}");
        assert_eq!(
            serde_json::from_slice::<Value>(&output).expect("empty JSON")["result"]["visible"]["bytes"],
            ""
        );

        let (code, _, _) = run_args(&["--store", &store_text, "project", "--budget", "nan"], b"");
        assert_eq!(code, 2);
    }

    #[test]
    fn global_and_projection_parsers_enforce_every_budget_boundary() {
        let mut projection = VecDeque::from([
            "--budget".to_owned(),
            "900".to_owned(),
            "--reserve".to_owned(),
            "300".to_owned(),
            "--unit".to_owned(),
            "tokens".to_owned(),
            "--profile".to_owned(),
            "diagnostic/v1".to_owned(),
            "--ttl".to_owned(),
            "60".to_owned(),
            "--json".to_owned(),
        ]);
        let parsed = parse_projection(&mut projection).expect("complete projection");
        assert_eq!(parsed.budget.unit, CountUnit::Tokens);
        assert_eq!(parsed.budget.total_visible_limit, 900);
        assert_eq!(parsed.budget.reserved_envelope, 300);
        assert_eq!(parsed.budget.token_profile.as_deref(), Some(CL100K_PROFILE));
        assert_eq!(parsed.profile, "diagnostic/v1");
        assert_eq!(parsed.retention.ttl_seconds, Some(60));
        assert!(parsed.json);
        assert!(projection.is_empty());

        let mut invalid_unit = VecDeque::from([
            "--budget".to_owned(),
            "1".to_owned(),
            "--unit".to_owned(),
            "words".to_owned(),
        ]);
        assert!(parse_projection(&mut invalid_unit).is_err());

        let mut duplicate_root = VecDeque::from([
            "--store".to_owned(),
            "/tmp/store.db".to_owned(),
            "--root".to_owned(),
            "workspace=/tmp/one".to_owned(),
            "--root".to_owned(),
            "workspace=/tmp/two".to_owned(),
        ]);
        assert!(parse_global(&mut duplicate_root).is_err());

        let mut malformed_root = VecDeque::from(["--root".to_owned(), "workspace".to_owned()]);
        assert!(parse_global(&mut malformed_root).is_err());

        let mut empty_root = VecDeque::from(["--root".to_owned(), "=/tmp/workspace".to_owned()]);
        assert!(parse_global(&mut empty_root).is_err());
    }

    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn closed_output_pipe_has_documented_exit_without_stack_trace() {
        let code = run(
            vec![OsString::from("--help")],
            b"".as_slice(),
            ClosedPipe,
            Vec::new(),
        );
        assert_eq!(code, BROKEN_PIPE_EXIT);
    }
}
