use super::*;
use distill::CL100K_PROFILE;
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
fn read_options_are_order_independent() {
    let temp = TempDir::new().expect("temp");
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    fs::write(workspace.join("sample.txt"), b"safe file").expect("file");
    let store = store(&temp);
    let root = format!("workspace={}", workspace.display());

    let (code, output, diagnostics) = run_args(
        &[
            "--store",
            store.to_str().expect("store path"),
            "--root",
            &root,
            "read",
            "--budget",
            "16",
            "--root-id",
            "workspace",
            "--path",
            "sample.txt",
            "--json",
        ],
        &[],
    );

    assert_eq!(code, 0, "{diagnostics}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output).expect("JSON")["ok"],
        true
    );
}

#[test]
fn run_options_are_order_independent_before_the_process_delimiter() {
    let temp = TempDir::new().expect("temp");
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let store = store(&temp);
    let root = format!("workspace={}", workspace.display());

    let (code, output, diagnostics) = run_args(
        &[
            "--store",
            store.to_str().expect("store path"),
            "--root",
            &root,
            "run",
            "--budget",
            "64",
            "--cwd-root",
            "workspace",
            "--timeout",
            "1000",
            "--cwd",
            ".",
            "--json",
            "--",
            "/usr/bin/printf",
            "ok",
        ],
        &[],
    );

    assert_eq!(code, 0, "{diagnostics}");
    let result: Value = serde_json::from_slice(&output).expect("JSON");
    assert_eq!(result["result"]["visible"]["bytes"], "ok");
}

#[test]
fn unknown_roots_use_the_engine_failure_contract() {
    let temp = TempDir::new().expect("temp");
    let store = store(&temp);
    let store_text = store.to_string_lossy();
    let fixture: Value = serde_json::from_str(include_str!(
        "../../docs/integrations/cli-conformance-v2.json"
    ))
    .expect("CLI conformance fixture");
    assert_eq!(fixture["schema_version"], "distill.cli-conformance/v2");
    assert_eq!(fixture["surface_schema_version"], CLI_SCHEMA_VERSION);
    let cases = fixture["configured_root_policy"]["cases"]
        .as_array()
        .expect("configured-root cases");

    for (operation, arguments) in [
        (
            "read",
            vec![
                "--store",
                &store_text,
                "read",
                "--root-id",
                "missing",
                "--path",
                "sample.txt",
                "--budget",
                "64",
                "--json",
            ],
        ),
        (
            "run",
            vec![
                "--store",
                &store_text,
                "run",
                "--cwd-root",
                "missing",
                "--cwd",
                ".",
                "--budget",
                "64",
                "--json",
                "--",
                "/usr/bin/printf",
                "ok",
            ],
        ),
    ] {
        let (code, output, diagnostics) = run_args(&arguments, b"");
        assert_eq!(code, 2, "{diagnostics}");
        let response: Value = serde_json::from_slice(&output).expect("failure JSON");
        assert_eq!(response["schema_version"], CLI_SCHEMA_VERSION);
        let expected = cases
            .iter()
            .find(|case| case["operation"] == operation)
            .expect("operation conformance case");
        assert_eq!(response["error"]["code"], expected["unknown_root_failure"]);
    }
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
            "%s%s",
            "$(not-a-shell)",
            "--json",
        ],
        b"",
    );
    assert_eq!(run_code, 0, "{run_error}");
    assert_eq!(
        String::from_utf8(run_output).expect("process output"),
        "$(not-a-shell)--json"
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

    let (code, output, stderr) = run_args(&["--store", &store_text, "artifact", "get", "bad"], b"");
    assert_eq!(code, 2);
    assert!(output.is_empty());
    assert!(!stderr.contains("stack"));

    let (code, output, stderr) = run_args(&["project", "--json"], b"");
    assert_eq!(code, 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&output).expect("invalid JSON")["ok"],
        false
    );
    assert!(stderr.is_empty());

    let (code, output, stderr) = run_args(&["--json", "unknown"], b"");
    assert_eq!(code, 2);
    assert!(output.is_empty());
    assert!(stderr.contains("unknown command '--json'"));
}

#[test]
fn child_options_after_process_delimiter_cannot_select_json_mode() {
    let temp = TempDir::new().expect("temp");
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let root = format!("workspace={}", workspace.display());
    let store = store(&temp);
    let store_text = store.to_string_lossy();
    let (code, output, stderr) = run_args(
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
            "/definitely/missing/distill-command",
            "--json",
            "--budget",
        ],
        b"",
    );

    assert_eq!(code, 6);
    assert!(output.is_empty());
    assert!(stderr.contains("acquisition_failed"));
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
    let mut json_mode = false;
    let parsed = parse_projection(&mut projection, &mut json_mode).expect("complete projection");
    assert_eq!(parsed.budget.unit, CountUnit::Tokens);
    assert_eq!(parsed.budget.total_visible_limit, 900);
    assert_eq!(parsed.budget.reserved_envelope, 300);
    assert_eq!(parsed.budget.token_profile.as_deref(), Some(CL100K_PROFILE));
    assert_eq!(parsed.profile, "diagnostic/v1");
    assert_eq!(parsed.retention.ttl_seconds, Some(60));
    assert!(parsed.json);
    assert!(json_mode);
    assert!(projection.is_empty());

    let mut invalid_unit = VecDeque::from([
        "--budget".to_owned(),
        "1".to_owned(),
        "--unit".to_owned(),
        "words".to_owned(),
    ]);
    assert!(parse_projection(&mut invalid_unit, &mut false).is_err());

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

#[test]
fn output_and_argument_helpers_cover_both_outcomes() {
    let output_error = SurfaceError::output(io::Error::other("write failed"));
    assert_eq!(output_error.exit_code, 70);
    assert_eq!(output_error.code, "output_failure");
    assert_eq!(output_error.message, "cannot write command output");

    assert_eq!(normalize_root_relative(".".to_owned()), "");
    assert_eq!(
        normalize_root_relative("nested".to_owned()),
        "nested".to_owned()
    );

    let mut args = VecDeque::from(["--json".to_owned()]);
    assert!(remove_flag(&mut args, "--json"));
    assert!(!remove_flag(&mut args, "--json"));
    ensure_empty(&args).expect("empty arguments");
    args.push_back("unexpected".to_owned());
    assert_eq!(
        ensure_empty(&args)
            .expect_err("unexpected argument")
            .exit_code,
        2
    );
}
