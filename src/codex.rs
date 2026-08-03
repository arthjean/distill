use crate::surface::{
    Recovery, SurfaceError, adapter_failure, bounded_correlation_id, bounded_focus, budget_for,
    codex_error_envelope, count_visible, default_request, projection_envelope, write_json_line,
};
use distill::{
    ByteString, CountUnit, Engine, EngineConfig, Failure, FailureCode, Fidelity, Outcome, Source,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
};

pub(crate) const HOOK_SCHEMA_VERSION: &str = "codex.post-tool-use/v2";
const PROJECTION_SCHEMA_VERSION: &str = "distill.codex-projection/v1";
pub(crate) const HOST_OUTPUT_CAP_TOKENS: u64 = 2_500;
pub(crate) const SAFE_OUTPUT_CAP_TOKENS: u64 = HOST_OUTPUT_CAP_TOKENS * 9 / 10;
pub(crate) const SETUP_MATCHER: &str = "*";
pub(crate) const SETUP_TIMEOUT_SECONDS: u64 = 30;
pub(crate) const SETUP_STATUS_MESSAGE: &str = "Distill context projection v1";
#[cfg(test)]
pub(crate) const SUPPORTED_MODES: &[&str] = &["off", "observe", "active"];
const DEFAULT_RESERVED_TOKENS: u64 = 450;
const MAX_HOOK_INPUT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Observe,
    Active,
}

impl Mode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "observe" => Some(Self::Observe),
            "active" => Some(Self::Active),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Active => "active",
        }
    }
}

pub(crate) fn setup_hook_arguments(mode: Mode) -> Vec<String> {
    vec![
        "codex-hook".to_owned(),
        "--mode".to_owned(),
        mode.as_str().to_owned(),
    ]
}

#[derive(Debug, Deserialize)]
struct PostToolUseEvent {
    #[serde(default, alias = "hook_schema_version")]
    schema_version: Option<String>,
    session_id: String,
    #[serde(default)]
    turn_id: Option<String>,
    cwd: String,
    hook_event_name: String,
    tool_name: String,
    tool_use_id: String,
    tool_input: Value,
    tool_response: Value,
}

pub(crate) fn run<R: Read, W: Write>(
    config: EngineConfig,
    mut args: VecDeque<String>,
    input: &mut R,
    output: &mut W,
) -> Result<(), SurfaceError> {
    let mut mode = None;
    let mut total_tokens = SAFE_OUTPUT_CAP_TOKENS;
    let mut reserved_tokens = DEFAULT_RESERVED_TOKENS;
    while let Some(argument) = args.pop_front() {
        match argument.as_str() {
            "--mode" => {
                mode = args.pop_front().as_deref().and_then(Mode::parse);
                if mode.is_none() {
                    return Err(SurfaceError::invalid(
                        "--mode requires off, observe, or active",
                    ));
                }
            }
            "--budget" => {
                total_tokens = parse_u64(args.pop_front(), "--budget")?;
            }
            "--reserve" => {
                reserved_tokens = parse_u64(args.pop_front(), "--reserve")?;
            }
            _ => {
                return Err(SurfaceError::invalid(format!(
                    "unexpected codex-hook argument '{argument}'"
                )));
            }
        }
    }
    let mode = mode.ok_or_else(|| SurfaceError::invalid("--mode is required"))?;
    if mode == Mode::Off {
        return Ok(());
    }
    if total_tokens > SAFE_OUTPUT_CAP_TOKENS || reserved_tokens > total_tokens {
        return write_feedback(
            output,
            &compact_feedback(
                FailureCode::InvalidRequest,
                "Codex adapter budget exceeds its documented 10% safety margin",
                None,
            ),
        );
    }

    let event = match read_event(input) {
        Ok(event) => event,
        Err((code, message)) => {
            return write_feedback(output, &compact_feedback(code, message, None));
        }
    };
    if let Err((code, message)) = validate_event(&event) {
        return write_feedback(output, &compact_feedback(code, message, None));
    }
    if is_unsupported_surface(&event.tool_name) {
        return write_json_line(
            output,
            &json!({
                "systemMessage": format!(
                    "Distill unsupported_surface: '{}' is not a supported local PostToolUse surface",
                    event.tool_name
                )
            }),
        );
    }

    let source = match response_bytes(&event.tool_response) {
        Ok(source) => source,
        Err(failure) => return write_mode_failure(output, mode, &failure),
    };
    let engine = match Engine::new(config) {
        Ok(engine) => engine,
        Err(failure) => return write_mode_failure(output, mode, &failure),
    };
    let request = default_request(
        bounded_request_id(&event),
        Source::Inline {
            bytes: ByteString::from(source),
            media_type: Some("application/json".to_owned()),
        },
        budget_for(CountUnit::Tokens, total_tokens, reserved_tokens),
        derived_focus(&event.tool_input),
    );
    let handled = catch_unwind(AssertUnwindSafe(|| engine.handle(request)));
    let outcome = match handled {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(failure)) => return write_mode_failure(output, mode, &failure),
        Err(_) => {
            let feedback = compact_feedback(
                FailureCode::InvariantBreach,
                "projection engine terminated unexpectedly; raw over-budget output was not marked projected",
                None,
            );
            return if mode == Mode::Active {
                write_feedback(output, &feedback)
            } else {
                write_json_line(
                    output,
                    &json!({"systemMessage": "Distill observe diagnostic [invariant_breach]"}),
                )
            };
        }
    };
    if mode == Mode::Observe || outcome.receipt.fidelity == Fidelity::Exact {
        return Ok(());
    }
    let projection = render_projection(&outcome);
    if token_count(&projection) > total_tokens {
        return write_feedback(
            output,
            &compact_feedback(
                FailureCode::InvariantBreach,
                "projected hook envelope exceeded the configured host-safe limit",
                Some(&outcome.artifact),
            ),
        );
    }
    write_feedback(output, &projection)
}

fn read_event<R: Read>(input: &mut R) -> Result<PostToolUseEvent, (FailureCode, &'static str)> {
    let mut bytes = Vec::new();
    input
        .take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| (FailureCode::InvalidRequest, "cannot read hook input"))?;
    if bytes.len() as u64 > MAX_HOOK_INPUT_BYTES {
        return Err((
            FailureCode::InputTooLarge,
            "hook input exceeds the 16 MiB protocol limit",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        (
            FailureCode::InvalidRequest,
            "PostToolUse hook JSON is malformed",
        )
    })
}

fn validate_event(event: &PostToolUseEvent) -> Result<(), (FailureCode, &'static str)> {
    if event
        .schema_version
        .as_deref()
        .is_some_and(|version| version != HOOK_SCHEMA_VERSION)
    {
        return Err((
            FailureCode::SchemaUnsupported,
            "unsupported Codex hook schema version",
        ));
    }
    if event.hook_event_name != "PostToolUse" {
        return Err((
            FailureCode::SchemaUnsupported,
            "hook event is not PostToolUse",
        ));
    }
    if event.session_id.is_empty()
        || event.session_id.len() > 256
        || event.cwd.is_empty()
        || event.cwd.len() > 4_096
        || event.tool_name.is_empty()
        || event.tool_name.len() > 256
        || event.tool_use_id.is_empty()
        || event.tool_use_id.len() > 256
        || event.turn_id.as_ref().is_some_and(|turn| turn.len() > 256)
    {
        return Err((
            FailureCode::InvalidRequest,
            "PostToolUse metadata is missing or exceeds its bounds",
        ));
    }
    if event.tool_input.is_null() {
        return Err((
            FailureCode::InvalidRequest,
            "PostToolUse tool_input is missing",
        ));
    }
    Ok(())
}

fn response_bytes(response: &Value) -> Result<Vec<u8>, Failure> {
    if let Some(text) = response.as_str() {
        return Ok(text.as_bytes().to_vec());
    }
    serde_json::to_vec(response).map_err(|_| {
        adapter_failure(
            FailureCode::InvariantBreach,
            "Codex tool response cannot be serialized",
            None,
        )
    })
}

pub(crate) fn is_unsupported_surface(tool_name: &str) -> bool {
    !matches!(
        tool_name,
        "Bash" | "apply_patch" | "Edit" | "Write" | "update_plan" | "Agent"
    ) && !tool_name.starts_with("mcp__")
}

/// The `tool_input` fields that state what a call was for, in the order they are
/// joined. The list is an allowlist rather than a walk of the object, because a
/// tool input also carries payloads: a focus states the intent of a call, never
/// the body it wrote.
pub(crate) const FOCUS_FIELDS: &[&str] = &["command", "file_path", "path", "pattern", "query"];

/// Derives the focus of a hook request from the tool input Codex recorded.
///
/// The hook is the one caller that always knows why an observation exists: the
/// model asked for it by name. Nothing here can fail the projection: an input
/// that is absent, structured differently, or oversized simply yields no focus,
/// and the request proceeds exactly as it did without one.
fn derived_focus(tool_input: &Value) -> Option<String> {
    if let Some(text) = tool_input.as_str() {
        return bounded_focus(text.to_owned());
    }
    let object = tool_input.as_object()?;
    let derived = FOCUS_FIELDS
        .iter()
        .filter_map(|field| object.get(*field)?.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    bounded_focus(derived)
}

fn bounded_request_id(event: &PostToolUseEvent) -> String {
    bounded_correlation_id(format!("{}:{}", event.session_id, event.tool_use_id))
}

fn render_projection(outcome: &Outcome) -> String {
    serde_json::to_string(&projection_envelope(
        PROJECTION_SCHEMA_VERSION,
        outcome,
        None,
        Recovery::CliCommands,
    ))
    .unwrap_or_else(|_| {
        compact_feedback(
            FailureCode::InvariantBreach,
            "projection envelope serialization failed",
            Some(&outcome.artifact),
        )
    })
}

fn failure_feedback(failure: &Failure) -> String {
    compact_feedback(
        failure.code,
        &failure.safe_message,
        failure.artifact.as_ref(),
    )
}

fn write_mode_failure<W: Write>(
    output: &mut W,
    mode: Mode,
    failure: &Failure,
) -> Result<(), SurfaceError> {
    if mode == Mode::Active {
        write_feedback(output, &failure_feedback(failure))
    } else {
        write_observe_diagnostic(output, failure)
    }
}

fn compact_feedback(
    code: FailureCode,
    message: &str,
    artifact: Option<&distill::ArtifactRef>,
) -> String {
    codex_error_envelope(PROJECTION_SCHEMA_VERSION, code.as_str(), message, artifact)
}

fn write_feedback<W: Write>(output: &mut W, feedback: &str) -> Result<(), SurfaceError> {
    write_json_line(
        output,
        &json!({
            "decision": "block",
            "reason": feedback,
        }),
    )
}

fn write_observe_diagnostic<W: Write>(
    output: &mut W,
    failure: &Failure,
) -> Result<(), SurfaceError> {
    write_json_line(
        output,
        &json!({
            "systemMessage": format!(
                "Distill observe diagnostic [{}]: {}",
                failure.code.as_str(),
                failure.safe_message.chars().take(256).collect::<String>()
            )
        }),
    )
}

fn token_count(text: &str) -> u64 {
    count_visible(text, CountUnit::Tokens)
}

fn parse_u64(value: Option<String>, flag: &str) -> Result<u64, SurfaceError> {
    value
        .ok_or_else(|| SurfaceError::invalid(format!("{flag} requires a value")))?
        .parse()
        .map_err(|_| SurfaceError::invalid(format!("{flag} requires an unsigned integer")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn config(temp: &TempDir) -> EngineConfig {
        EngineConfig::local(temp.path().join("store/artifacts.db"))
    }

    fn event(tool: &str, response: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": HOOK_SCHEMA_VERSION,
            "session_id": "session",
            "turn_id": "turn",
            "cwd": "/workspace",
            "hook_event_name": "PostToolUse",
            "permission_mode": "default",
            "tool_name": tool,
            "tool_use_id": "call",
            "tool_input": {"command": "printf"},
            "tool_response": response,
            "model": "gpt-test",
            "transcript_path": null,
        }))
        .expect("event")
    }

    fn invoke(
        config: EngineConfig,
        mode: &str,
        mut input: &[u8],
    ) -> (Result<(), SurfaceError>, Vec<u8>) {
        let mut output = Vec::new();
        let result = run(
            config,
            VecDeque::from(["--mode".to_owned(), mode.to_owned()]),
            &mut input,
            &mut output,
        );
        (result, output)
    }

    #[test]
    fn modes_capture_and_replace_only_as_documented() {
        let temp = TempDir::new().expect("temp");
        let off_config = config(&temp);
        let store = off_config.store_path.clone();
        let (result, output) = invoke(off_config, "off", b"not JSON");
        assert!(result.is_ok());
        assert!(output.is_empty());
        assert!(!store.exists());

        let large = (0..1_000)
            .map(|index| format!("line-{index:04} secret-middle-{index}\n"))
            .collect::<String>();
        let (result, output) = invoke(config(&temp), "observe", &event("Bash", json!(large)));
        assert!(result.is_ok());
        assert!(output.is_empty());

        let (result, output) = invoke(config(&temp), "active", &event("Bash", json!(large)));
        assert!(result.is_ok());
        let response: Value = serde_json::from_slice(&output).expect("active response");
        assert_eq!(response["decision"], "block");
        let feedback = response["reason"].as_str().expect("feedback");
        assert!(feedback.contains(PROJECTION_SCHEMA_VERSION));
        assert!(feedback.contains("artifact"));
        assert!(!feedback.contains("secret-middle-500"));
        assert!(token_count(feedback) <= SAFE_OUTPUT_CAP_TOKENS);
        // US-009: the envelope, including its bounded recovery instruction and
        // its omission accounting, still fits the reserved allowance the engine
        // was told to subtract.
        let envelope: Value = serde_json::from_str(feedback).expect("projection envelope");
        let projection = envelope["projection"].as_str().expect("projection");
        assert!(
            envelope["recovery"]
                .as_str()
                .expect("recovery")
                .contains("distill artifact slice")
        );
        assert!(envelope["accounting"]["omitted"].as_u64().expect("omitted") > 0);
        assert!(
            token_count(feedback) - token_count(projection) <= DEFAULT_RESERVED_TOKENS,
            "hook envelope overhead exceeds its reserved allowance"
        );

        let (result, output) = invoke(config(&temp), "active", &event("Bash", json!("small")));
        assert!(result.is_ok());
        assert!(output.is_empty());
    }

    #[test]
    fn malformed_unknown_and_unsupported_events_are_actionable() {
        let temp = TempDir::new().expect("temp");
        let (_, malformed) = invoke(config(&temp), "active", b"{");
        let response: Value = serde_json::from_slice(&malformed).expect("malformed feedback");
        assert_eq!(response["decision"], "block");
        assert!(
            response["reason"]
                .as_str()
                .expect("reason")
                .contains("invalid_request")
        );

        let mut unknown: Value =
            serde_json::from_slice(&event("Bash", json!("large"))).expect("event value");
        unknown["schema_version"] = json!("codex.post-tool-use/v9");
        let (_, output) = invoke(
            config(&temp),
            "active",
            &serde_json::to_vec(&unknown).expect("unknown event"),
        );
        assert!(
            String::from_utf8(output)
                .expect("unknown output")
                .contains("schema_unsupported")
        );

        let (_, output) = invoke(
            config(&temp),
            "active",
            &event("WebSearch", json!("hosted")),
        );
        let response: Value = serde_json::from_slice(&output).expect("unsupported response");
        assert!(
            response["systemMessage"]
                .as_str()
                .expect("message")
                .contains("unsupported_surface")
        );

        let (_, output) = invoke(
            config(&temp),
            "active",
            &event("FutureHostedTool", json!("hosted")),
        );
        let response: Value = serde_json::from_slice(&output).expect("unknown surface response");
        assert!(
            response["systemMessage"]
                .as_str()
                .expect("message")
                .contains("unsupported_surface")
        );
    }

    #[test]
    fn conformance_categories_share_the_inline_engine_path() {
        let categories = [
            "Bash",
            "apply_patch",
            "mcp__filesystem__read_file",
            "update_plan",
            "Agent",
        ];
        for (index, tool) in categories.into_iter().enumerate() {
            let temp = TempDir::new().expect("temp");
            let response = format!(
                "{}\nMIDDLE-{index}\n{}",
                "x".repeat(12_000),
                "z".repeat(12_000)
            );
            let (result, output) = invoke(config(&temp), "active", &event(tool, json!(response)));
            assert!(result.is_ok(), "{tool}");
            let value: Value = serde_json::from_slice(&output).expect("hook response");
            assert_eq!(value["decision"], "block", "{tool}");
        }
    }

    /// US-017: the hook derives a bounded focus from the tool input Codex
    /// recorded, and an input it cannot read simply yields none.
    #[test]
    fn focus_is_derived_from_tool_input_or_omitted() {
        assert_eq!(
            derived_focus(&json!({"command": "cargo test --locked projection"})),
            Some("cargo test --locked projection".to_owned())
        );
        assert_eq!(
            derived_focus(&json!({"file_path": "src/projection.rs", "path": "src"})),
            Some("src/projection.rs src".to_owned())
        );
        assert_eq!(
            derived_focus(&json!("read the release protocol")),
            Some("read the release protocol".to_owned())
        );

        // A payload field is not intent: a focus states why a call was made,
        // never the body it wrote.
        assert_eq!(
            derived_focus(&json!({"content": "the entire file body", "input": "*** patch"})),
            None
        );
        for undecodable in [
            json!(null),
            json!(42),
            json!([1, 2]),
            json!({}),
            json!("   "),
        ] {
            assert_eq!(derived_focus(&undecodable), None, "{undecodable}");
        }

        // An oversized input is truncated on a UTF-8 boundary inside the bound,
        // because the hook is the author of the value rather than its caller.
        let oversized = format!("{}é", "cargo ".repeat(distill::MAX_FOCUS_BYTES));
        let derived = derived_focus(&json!({"command": oversized})).expect("truncated focus");
        assert!(derived.len() <= distill::MAX_FOCUS_BYTES);
        assert!(derived.is_char_boundary(derived.len()));
        let multibyte = format!("{}é", "x".repeat(distill::MAX_FOCUS_BYTES - 1));
        let derived = derived_focus(&json!({"command": multibyte})).expect("multibyte focus");
        assert_eq!(derived.len(), distill::MAX_FOCUS_BYTES - 1);
    }

    /// US-017: the derived focus reaches the engine, which records that it
    /// applied without echoing it into the model-visible envelope.
    #[test]
    fn a_derived_focus_reaches_the_engine_without_entering_the_envelope() {
        let temp = TempDir::new().expect("temp");
        // Long lines without quotes: the observation stays large while its
        // projection carries few lines, so the JSON envelope escapes little.
        let line = |index: usize| {
            format!(
                "let value_{index:03} = recompute_projection_budget(value_{index:03}, attempts_remaining, retained_span_count);\n"
            )
        };
        let body = format!(
            "use crate::artifact::Receipt;\n{}let RETRY_BUDGET = attempts.saturating_add(3);\n{}",
            (0..400).map(line).collect::<String>(),
            (400..800).map(line).collect::<String>(),
        );
        let mut event: Value =
            serde_json::from_slice(&event("Bash", json!(body))).expect("event value");
        event["tool_input"] = json!({"command": "rg RETRY_BUDGET src"});
        let (result, output) = invoke(
            config(&temp),
            "active",
            &serde_json::to_vec(&event).expect("focused event"),
        );
        assert!(result.is_ok());

        let response: Value = serde_json::from_slice(&output).expect("hook response");
        let feedback = response["reason"].as_str().expect("feedback");
        let envelope: Value = serde_json::from_str(feedback).expect("projection envelope");
        let projection = envelope["projection"].as_str().expect("projection");
        assert!(
            projection.contains("RETRY_BUDGET"),
            "the derived focus did not reach selection"
        );
        assert!(
            !feedback.contains("rg RETRY_BUDGET src"),
            "the envelope echoed the derived focus"
        );
        assert!(token_count(feedback) <= SAFE_OUTPUT_CAP_TOKENS);
    }

    #[test]
    fn object_tool_responses_preserve_sibling_metadata_before_projection() {
        let response = json!({
            "output": "command output",
            "exit_code": 9,
            "timed_out": false
        });
        let bytes = response_bytes(&response).expect("serialize response");
        let decoded: Value = serde_json::from_slice(&bytes).expect("complete response");
        assert_eq!(decoded, response);
    }

    #[test]
    fn invalid_adapter_budget_fails_closed() {
        let temp = TempDir::new().expect("temp");
        let mut output = Vec::new();
        let result = run(
            config(&temp),
            VecDeque::from([
                "--mode".to_owned(),
                "active".to_owned(),
                "--budget".to_owned(),
                HOST_OUTPUT_CAP_TOKENS.to_string(),
            ]),
            &mut event("Bash", json!("content")).as_slice(),
            &mut output,
        );
        assert!(result.is_ok());
        assert!(
            String::from_utf8(output)
                .expect("feedback")
                .contains("safety margin")
        );
    }

    #[test]
    fn argument_event_and_store_boundaries_fail_safely() {
        let temp = TempDir::new().expect("temp");
        let valid_event = event("Bash", json!("content"));

        let mut output = Vec::new();
        assert!(
            run(
                config(&temp),
                VecDeque::new(),
                &mut valid_event.as_slice(),
                &mut output,
            )
            .is_err()
        );

        let mut output = Vec::new();
        assert!(
            run(
                config(&temp),
                VecDeque::from(["--mode".to_owned(), "invalid".to_owned()]),
                &mut valid_event.as_slice(),
                &mut output,
            )
            .is_err()
        );

        let mut output = Vec::new();
        assert!(
            run(
                config(&temp),
                VecDeque::from(["unexpected".to_owned()]),
                &mut valid_event.as_slice(),
                &mut output,
            )
            .is_err()
        );

        let mut output = Vec::new();
        run(
            config(&temp),
            VecDeque::from([
                "--mode".to_owned(),
                "active".to_owned(),
                "--budget".to_owned(),
                "1000".to_owned(),
                "--reserve".to_owned(),
                "1001".to_owned(),
            ]),
            &mut valid_event.as_slice(),
            &mut output,
        )
        .expect("closed budget response");
        assert!(
            String::from_utf8(output)
                .expect("feedback")
                .contains("safety margin")
        );

        let mut parsed: PostToolUseEvent =
            serde_json::from_slice(&valid_event).expect("valid event");
        parsed.schema_version = None;
        assert!(validate_event(&parsed).is_ok());
        parsed.hook_event_name = "PreToolUse".to_owned();
        assert!(validate_event(&parsed).is_err());
        parsed.hook_event_name = "PostToolUse".to_owned();
        parsed.session_id.clear();
        assert!(validate_event(&parsed).is_err());
        parsed.session_id = "session".to_owned();
        parsed.cwd.clear();
        assert!(validate_event(&parsed).is_err());
        parsed.cwd = "/workspace".to_owned();
        parsed.tool_name.clear();
        assert!(validate_event(&parsed).is_err());
        parsed.tool_name = "Bash".to_owned();
        parsed.tool_use_id.clear();
        assert!(validate_event(&parsed).is_err());
        parsed.tool_use_id = "call".to_owned();
        parsed.turn_id = Some("x".repeat(257));
        assert!(validate_event(&parsed).is_err());
        parsed.turn_id = None;
        parsed.tool_input = Value::Null;
        assert!(validate_event(&parsed).is_err());

        let unusable_store = temp.path().join("store-is-directory");
        fs::create_dir(&unusable_store).expect("unusable store");
        let bad_config = EngineConfig::local(unusable_store);
        let (observe_result, observe_output) = invoke(bad_config.clone(), "observe", &valid_event);
        assert!(observe_result.is_ok());
        assert!(
            String::from_utf8(observe_output)
                .expect("observe diagnostic")
                .contains("systemMessage")
        );
        let (active_result, active_output) = invoke(bad_config, "active", &valid_event);
        assert!(active_result.is_ok());
        assert!(
            String::from_utf8(active_output)
                .expect("active feedback")
                .contains("\"decision\":\"block\"")
        );
    }
}
