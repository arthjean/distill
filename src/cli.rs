use crate::{
    codex, mcp, setup,
    surface::{BROKEN_PIPE_EXIT, SurfaceError, write_json_line},
};
use distill::{
    ArtifactRef, BinaryPolicy, Budget, ByteString, CL100K_PROFILE, CONTRACT_VERSION, CountUnit,
    Engine, EngineConfig, Failure, FailureCode, MAX_SOURCE_BYTES, Outcome, Request, Retention,
    Source,
};
use serde::Serialize;
#[cfg(test)]
use serde_json::Value;
use serde_json::json;
#[cfg(test)]
use std::io;
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    io::{Read, Write},
    path::PathBuf,
};

const CLI_SCHEMA_VERSION: &str = "distill.cli/v1";

const HELP: &str = r#"distill: local bounded context projection

Usage:
  distill [--store PATH] [--root ID=PATH] project --budget N [--json]
  distill [--store PATH] artifact get ID [--json]
  distill [--store PATH] artifact trace ID [--json]
  distill [--store PATH] status [--json]
  distill [--store PATH] gc [--json]
  distill [--store PATH] [--root ID=PATH] read --root-id ID --path PATH --budget N [--json]
  distill [--store PATH] [--root ID=PATH] run --cwd-root ID --cwd PATH --budget N [--timeout MS] [--json] -- EXECUTABLE [ARG...]
  distill [--store PATH] codex-hook --mode off|observe|active
  distill [--store PATH] [--root ID=PATH] mcp
  distill setup codex --config PATH --command PATH [--store PATH] [--mode off|observe|active] [--dry-run|--restore]
  distill setup claude --config PATH --command PATH [--store PATH] [--root ID=PATH]... [--dry-run|--restore]

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
struct ProjectionParser {
    total: Option<u64>,
    reserve: u64,
    unit: CountUnit,
    profile: String,
    ttl_seconds: Option<u64>,
    json: bool,
}

impl ProjectionParser {
    fn new() -> Self {
        Self {
            total: None,
            reserve: 0,
            unit: CountUnit::Bytes,
            profile: "plain-text/v1".to_owned(),
            ttl_seconds: None,
            json: false,
        }
    }

    fn consume(
        &mut self,
        argument: &str,
        args: &mut VecDeque<String>,
        json_mode: &mut bool,
    ) -> Result<(), SurfaceError> {
        match argument {
            "--budget" => {
                self.total = Some(parse_u64(&take(args, "--budget")?, "--budget")?);
            }
            "--reserve" => {
                self.reserve = parse_u64(&take(args, "--reserve")?, "--reserve")?;
            }
            "--unit" => {
                self.unit = match take(args, "--unit")?.as_str() {
                    "bytes" => CountUnit::Bytes,
                    "tokens" => CountUnit::Tokens,
                    _ => return Err(SurfaceError::invalid("--unit must be bytes or tokens")),
                };
            }
            "--profile" => self.profile = take(args, "--profile")?,
            "--ttl" => {
                self.ttl_seconds = Some(parse_u64(&take(args, "--ttl")?, "--ttl")?);
            }
            "--json" => {
                self.json = true;
                *json_mode = true;
            }
            _ => {
                return Err(SurfaceError::invalid(format!(
                    "unexpected argument '{argument}'"
                )));
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<ProjectionOptions, SurfaceError> {
        let total_visible_limit = self
            .total
            .ok_or_else(|| SurfaceError::invalid("--budget is required"))?;
        Ok(ProjectionOptions {
            budget: Budget {
                unit: self.unit,
                total_visible_limit,
                reserved_envelope: self.reserve,
                token_profile: (self.unit == CountUnit::Tokens).then(|| CL100K_PROFILE.to_owned()),
            },
            profile: self.profile,
            retention: Retention {
                expires_at: None,
                ttl_seconds: self.ttl_seconds,
            },
            json: self.json,
        })
    }
}

pub(crate) fn run<R: Read, W: Write, E: Write>(
    raw_args: Vec<OsString>,
    mut input: R,
    mut output: W,
    mut diagnostics: E,
) -> i32 {
    let mut json_mode = false;
    let result = run_inner(
        raw_args,
        &mut input,
        &mut output,
        &mut diagnostics,
        &mut json_mode,
    );
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
    json_mode: &mut bool,
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
        "project" => project(global, args, input, output, diagnostics, json_mode),
        "artifact" => artifact(global, args, output, json_mode),
        "status" => status(global, args, output, json_mode),
        "gc" => gc(global, args, output, json_mode),
        "read" => read_file(global, args, output, diagnostics, json_mode),
        "run" => run_process(global, args, output, diagnostics, json_mode),
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

fn parse_projection(
    args: &mut VecDeque<String>,
    json_mode: &mut bool,
) -> Result<ProjectionOptions, SurfaceError> {
    let mut parser = ProjectionParser::new();
    while let Some(argument) = args.pop_front() {
        parser.consume(&argument, args, json_mode)?;
    }
    parser.finish()
}

fn project<R: Read, W: Write, E: Write>(
    global: GlobalOptions,
    mut args: VecDeque<String>,
    input: &mut R,
    output: &mut W,
    diagnostics: &mut E,
    json_mode: &mut bool,
) -> Result<(), SurfaceError> {
    let options = parse_projection(&mut args, json_mode)?;
    ensure_empty(&args)?;
    let bytes = read_bounded(input, MAX_SOURCE_BYTES)?;
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
    json_mode: &mut bool,
) -> Result<(), SurfaceError> {
    let mut root_id = None;
    let mut path = None;
    let mut binary_policy = BinaryPolicy::Accept;
    let mut projection = ProjectionParser::new();
    while let Some(argument) = args.pop_front() {
        match argument.as_str() {
            "--root-id" => root_id = Some(take(&mut args, "--root-id")?),
            "--path" => path = Some(take(&mut args, "--path")?),
            "--reject-binary" => binary_policy = BinaryPolicy::Reject,
            _ => projection.consume(&argument, &mut args, json_mode)?,
        }
    }
    let options = projection.finish()?;
    let root_id = root_id.ok_or_else(|| SurfaceError::invalid("--root-id is required"))?;
    if !global.roots.contains_key(&root_id) {
        return Err(SurfaceError::invalid(
            "--root-id must name a configured --root",
        ));
    }
    let path = path.ok_or_else(|| SurfaceError::invalid("--path is required"))?;
    let request = request(
        "cli-read",
        Source::File {
            root_id,
            relative_path: ByteString::from_utf8(path),
            binary_policy,
        },
        options.clone(),
    );
    let outcome = Engine::new(global.config())?.handle(request)?;
    write_outcome(output, diagnostics, &outcome, options.json)
}

fn run_process<W: Write, E: Write>(
    global: GlobalOptions,
    mut args: VecDeque<String>,
    output: &mut W,
    diagnostics: &mut E,
    json_mode: &mut bool,
) -> Result<(), SurfaceError> {
    let mut cwd_root = None;
    let mut cwd = None;
    let mut timeout_ms = None;
    let mut environment_profile = None;
    let mut projection = ProjectionParser::new();
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
            _ => projection.consume(&argument, &mut args, json_mode)?,
        }
    }
    let options = projection.finish()?;
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
    json_mode: &mut bool,
) -> Result<(), SurfaceError> {
    let action = args
        .pop_front()
        .ok_or_else(|| SurfaceError::invalid("artifact requires get or trace"))?;
    let target = args
        .pop_front()
        .ok_or_else(|| SurfaceError::invalid("artifact requires an ID or JSON reference"))?;
    let json = remove_flag(&mut args, "--json");
    *json_mode |= json;
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
    json_mode: &mut bool,
) -> Result<(), SurfaceError> {
    *json_mode |= remove_flag(&mut args, "--json");
    ensure_empty(&args)?;
    write_success(output, &Engine::new(global.config())?.status()?)
}

fn gc<W: Write>(
    global: GlobalOptions,
    mut args: VecDeque<String>,
    output: &mut W,
    json_mode: &mut bool,
) -> Result<(), SurfaceError> {
    *json_mode |= remove_flag(&mut args, "--json");
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
#[path = "cli/tests.rs"]
mod tests;
