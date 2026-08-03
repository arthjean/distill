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

fn committed_artifact(store_text: &str, body: &str) -> String {
    let (code, output, stderr) = run_args(
        &["--store", store_text, "project", "--budget", "16", "--json"],
        body.as_bytes(),
    );
    assert_eq!(code, 0, "{stderr}");
    serde_json::from_slice::<Value>(&output).expect("projection JSON")["result"]["artifact"]["id"]
        .as_str()
        .expect("artifact ID")
        .to_owned()
}

/// US-010: retrieval is available on the CLI with the versioned JSON result and
/// the existing stable exit codes.
#[test]
fn artifact_slice_and_search_emit_versioned_results() {
    let temp = TempDir::new().expect("temp");
    let store = store(&temp);
    let store_text = store.to_string_lossy().into_owned();
    let body = "alpha\nbravo\nCLI_NEEDLE charlie\ndelta\necho\n";
    let id = committed_artifact(&store_text, body);

    let (code, output, stderr) = run_args(
        &[
            "--store",
            &store_text,
            "artifact",
            "slice",
            &id,
            "--start-line",
            "2",
            "--lines",
            "2",
            "--budget",
            "256",
            "--json",
        ],
        b"",
    );
    assert_eq!(code, 0, "{stderr}");
    let result: Value = serde_json::from_slice(&output).expect("slice JSON");
    assert_eq!(result["schema_version"], CLI_SCHEMA_VERSION);
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["result"]["visible"]["bytes"],
        "bravo\nCLI_NEEDLE charlie\n"
    );
    assert_eq!(
        result["result"]["receipt"]["original_count"],
        body.len() as u64
    );

    let (code, output, stderr) = run_args(
        &[
            "--store",
            &store_text,
            "artifact",
            "search",
            &id,
            "--pattern",
            "CLI_NEEDLE",
            "--before",
            "0",
            "--after",
            "0",
            "--budget",
            "256",
        ],
        b"",
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        String::from_utf8(output).expect("search payload"),
        "CLI_NEEDLE charlie\n"
    );

    // The unbounded operator path is unchanged and still returns the whole
    // committed source.
    let (code, output, stderr) = run_args(&["--store", &store_text, "artifact", "get", &id], b"");
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(String::from_utf8(output).expect("restored"), body);
}

/// US-010: a retrieval request the CLI cannot complete fails before the engine
/// opens the store, and selector values stay literal.
#[test]
fn retrieval_arguments_fail_closed_and_stay_literal() {
    let temp = TempDir::new().expect("temp");
    let unread_store = temp.path().join("unread/artifacts.db");
    let unread_text = unread_store.to_string_lossy().into_owned();
    let id = "0123456789abcdef0123456789abcdef";

    for arguments in [
        vec![
            "--store",
            &unread_text,
            "artifact",
            "slice",
            id,
            "--start-line",
            "1",
            "--lines",
            "1",
            "--json",
        ],
        vec![
            "--store",
            &unread_text,
            "artifact",
            "search",
            id,
            "--pattern",
            "needle",
            "--json",
        ],
    ] {
        let (code, output, _) = run_args(&arguments, b"");
        assert_eq!(code, 2);
        let failure: Value = serde_json::from_slice(&output).expect("failure JSON");
        assert_eq!(failure["error"]["code"], "invalid_input");
        assert!(
            failure["error"]["message"]
                .as_str()
                .expect("message")
                .contains("--budget")
        );
    }

    // An out-of-bounds selector is refused by the engine contract, still before
    // the store is opened.
    let (code, output, _) = run_args(
        &[
            "--store",
            &unread_text,
            "artifact",
            "slice",
            id,
            "--start-line",
            "0",
            "--lines",
            "1",
            "--budget",
            "64",
            "--json",
        ],
        b"",
    );
    assert_eq!(code, 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&output).expect("bounds JSON")["error"]["code"],
        "invalid_request"
    );

    // Each selector keeps only its own options.
    let (code, _, stderr) = run_args(
        &[
            "--store",
            &unread_text,
            "artifact",
            "slice",
            id,
            "--start-line",
            "1",
            "--lines",
            "1",
            "--pattern",
            "needle",
            "--budget",
            "64",
        ],
        b"",
    );
    assert_eq!(code, 2);
    assert!(stderr.contains("--start-line and --lines"));
    assert!(!unread_store.exists());

    // A selector value that looks like a Distill option remains literal and
    // cannot select Distill output mode.
    let store = store(&temp);
    let store_text = store.to_string_lossy().into_owned();
    let id = committed_artifact(&store_text, "alpha\n--json marker\nbravo\n");
    let (code, output, stderr) = run_args(
        &[
            "--store",
            &store_text,
            "artifact",
            "search",
            &id,
            "--pattern",
            "--json",
            "--before",
            "0",
            "--after",
            "0",
            "--budget",
            "256",
        ],
        b"",
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        String::from_utf8(output).expect("literal pattern payload"),
        "--json marker\n"
    );
}

/// US-010: the CLI conformance matrix pins the retrieval forms, the selector
/// bounds, and the surface schema the release ships.
#[test]
fn cli_conformance_matrix_pins_retrieval_and_selector_bounds() {
    let matrix: Value = serde_json::from_str(include_str!(
        "../../docs/integrations/cli-conformance-v3.json"
    ))
    .expect("CLI conformance fixture");
    assert_eq!(matrix["schema_version"], "distill.cli-conformance/v3");
    assert_eq!(matrix["supersedes"], "distill.cli-conformance/v2");
    assert_eq!(matrix["surface_schema_version"], CLI_SCHEMA_VERSION);
    assert_eq!(matrix["request_contract_version"], CONTRACT_VERSION);
    assert_eq!(matrix["retrieval"]["pattern_language"], "literal");

    // US-014: the CLI pins the same shape-derived preservation contract, and
    // every retired identifier it still accepts resolves to a shape policy.
    let preservation = &matrix["preservation"];
    assert_eq!(preservation["default_profile"], distill::AUTO_PROFILE);
    assert_eq!(
        preservation["default_profile"],
        crate::surface::DEFAULT_PRESERVATION_PROFILE
    );
    assert_eq!(preservation["policy_version"], distill::POLICY_VERSION);
    assert_eq!(
        preservation["receipt_schema_version"],
        distill::RECEIPT_SCHEMA_VERSION
    );
    assert_eq!(
        preservation["unsupported_profile_failure"],
        "invalid_request"
    );
    assert_eq!(
        matrix["retrieval"]["unbounded_recovery_command"],
        "distill artifact get"
    );

    let bounds = &matrix["retrieval"]["selector_bounds"];
    assert_eq!(
        bounds["max_pattern_bytes"],
        distill::MAX_SELECTOR_PATTERN_BYTES
    );
    assert_eq!(
        bounds["max_context_lines"],
        distill::MAX_SELECTOR_CONTEXT_LINES
    );
    assert_eq!(bounds["max_matches"], distill::MAX_SELECTOR_MATCHES);
    assert_eq!(
        bounds["default_context_lines"],
        distill::DEFAULT_SELECTOR_CONTEXT_LINES
    );
    assert_eq!(bounds["default_matches"], distill::DEFAULT_SELECTOR_MATCHES);

    // US-017: the focus is pinned as an optional literal option at the
    // contract bound, recorded by the receipt without its value.
    let focus = &matrix["focus"];
    assert_eq!(focus["optional"], true);
    assert_eq!(focus["max_bytes"], distill::MAX_FOCUS_BYTES);
    assert_eq!(focus["literal_value"], true);
    assert_eq!(focus["receipt_echoes_value"], false);
    let option = focus["option"].as_str().expect("focus option");
    assert!(HELP.contains(option), "help omits {option}");

    for command in matrix["retrieval"]["commands"]
        .as_array()
        .expect("retrieval commands")
    {
        let operation = command["operation"].as_str().expect("operation");
        let form = command["form"].as_str().expect("form");
        assert!(HELP.contains(&format!("artifact {operation} ID")), "{form}");
        assert_eq!(command["missing_budget_failure"], "invalid_input");
        assert_eq!(command["missing_budget_exit_code"], 2);
        for option in command["required_options"]
            .as_array()
            .expect("required options")
        {
            let option = option.as_str().expect("option");
            assert!(form.contains(option), "{form} omits {option}");
            assert!(HELP.contains(option), "help omits {option}");
        }
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

/// US-015 and US-017: `--focus` is optional, orders what the budget keeps, stays
/// literal, and fails with the engine's typed bound rather than being truncated.
#[test]
fn focus_is_optional_literal_and_bounded_on_the_cli() {
    let temp = TempDir::new().expect("temp");
    let store = store(&temp);
    let store = store.to_str().expect("store path");
    let line = |index: usize| format!("let value_{index:03} = compute(value_{index:03});\n");
    let body = format!(
        "use crate::artifact::Receipt;\n{}let RETRY_BUDGET = attempts;\n{}",
        (0..200).map(line).collect::<String>(),
        (200..400).map(line).collect::<String>(),
    );

    let project = |focus: Option<&str>| {
        let mut arguments = vec!["--store", store, "project", "--budget", "240", "--json"];
        if let Some(focus) = focus {
            arguments.extend(["--focus", focus]);
        }
        let (code, stdout, _) = run_args(&arguments, body.as_bytes());
        (code, stdout)
    };

    let (code, stdout) = project(None);
    assert_eq!(code, 0);
    let unfocused: Value = serde_json::from_slice(&stdout).expect("unfocused JSON");
    assert_eq!(
        unfocused["result"]["receipt"]["preservation"]["focus_applied"],
        false
    );
    assert!(
        !unfocused["result"]["visible"]["bytes"]
            .as_str()
            .expect("visible")
            .contains("RETRY_BUDGET")
    );

    let (code, stdout) = project(Some("why was the retry budget consumed"));
    assert_eq!(code, 0);
    let focused: Value = serde_json::from_slice(&stdout).expect("focused JSON");
    assert_eq!(
        focused["result"]["receipt"]["preservation"]["focus_applied"],
        true
    );
    assert!(
        focused["result"]["visible"]["bytes"]
            .as_str()
            .expect("visible")
            .contains("RETRY_BUDGET")
    );

    // A focus that looks like a Distill option is a focus.
    let (code, stdout) = project(Some("--json --budget 1"));
    assert_eq!(code, 0);
    let literal: Value = serde_json::from_slice(&stdout).expect("literal JSON");
    assert_eq!(
        literal["result"]["receipt"]["preservation"]["focus_applied"],
        true
    );

    let oversized = "f".repeat(distill::MAX_FOCUS_BYTES + 1);
    let (code, stdout) = project(Some(&oversized));
    assert_eq!(code, 2);
    let failure: Value = serde_json::from_slice(&stdout).expect("failure JSON");
    assert_eq!(failure["error"]["code"], "invalid_request");
}
