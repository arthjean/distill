use crate::surface::SurfaceError;
use distill::{
    BinaryPolicy, Budget, ByteString, CL100K_PROFILE, CONTRACT_VERSION, CountUnit, Engine,
    EngineConfig, Failure, FailureCode, MAX_IDENTIFIER_BYTES, MAX_PATH_BYTES,
    MAX_PROCESS_ARGUMENT_BYTES, MAX_PROCESS_ARGUMENTS, MAX_PROCESS_EXECUTABLE_BYTES,
    MAX_PROCESS_TIMEOUT_MS, MIN_PROCESS_TIMEOUT_MS, Outcome, Request, Retention, Source,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
};
use tiktoken_rs::cl100k_base_singleton;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const MCP_ADAPTER_VERSION: &str = "distill.mcp/v1";
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpBudget {
    unit: CountUnit,
    total_visible_limit: u64,
    #[serde(default)]
    reserved_envelope: Option<u64>,
}

impl McpBudget {
    fn engine_budget(&self) -> Budget {
        let default_reserve = match self.unit {
            CountUnit::Bytes => 1_024,
            CountUnit::Tokens => 400,
        };
        Budget {
            unit: self.unit,
            total_visible_limit: self.total_visible_limit,
            reserved_envelope: self.reserved_envelope.unwrap_or(default_reserve),
            token_profile: (self.unit == CountUnit::Tokens).then(|| CL100K_PROFILE.to_owned()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    root_id: String,
    path: String,
    budget: McpBudget,
    #[serde(default)]
    binary_policy: Option<BinaryPolicy>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArguments {
    executable: String,
    argv: Vec<String>,
    cwd_root_id: String,
    cwd: String,
    budget: McpBudget,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    environment_profile: Option<String>,
}

pub(crate) fn run<R: Read, W: Write, E: Write>(
    config: EngineConfig,
    input: &mut R,
    output: &mut W,
    _diagnostics: &mut E,
) -> Result<(), SurfaceError> {
    let engine = Engine::new(config)?;
    let mut reader = BufReader::new(input);
    loop {
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_MESSAGE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|_| SurfaceError::invalid("cannot read MCP stdio"))?;
        if read == 0 {
            return Ok(());
        }
        if line.len() > MAX_MESSAGE_BYTES {
            write_protocol_error(output, Value::Null, -32600, "MCP message exceeds 1 MiB")?;
            if line.last() != Some(&b'\n') {
                drain_line(&mut reader)?;
            }
            continue;
        }
        let request: JsonRpcRequest = match serde_json::from_slice(&line) {
            Ok(request) => request,
            Err(_) => {
                write_protocol_error(output, Value::Null, -32700, "invalid JSON")?;
                continue;
            }
        };
        if request.jsonrpc != "2.0" || request.method.is_empty() {
            write_protocol_error(
                output,
                request.id.unwrap_or(Value::Null),
                -32600,
                "invalid JSON-RPC request",
            )?;
            continue;
        }
        if request
            .id
            .as_ref()
            .is_some_and(|id| !id.is_string() && !id.is_number())
        {
            write_protocol_error(
                output,
                Value::Null,
                -32600,
                "JSON-RPC id must be a string, number, or null",
            )?;
            continue;
        }
        let id = request.id.clone();
        if id.is_none() {
            continue;
        }
        if let Some(response) = dispatch(&engine, request) {
            let id = id.unwrap_or(Value::Null);
            match response {
                Ok(result) => write_result(output, id, result)?,
                Err((code, message)) => write_protocol_error(output, id, code, message)?,
            }
        }
    }
}

fn dispatch(
    engine: &Engine,
    request: JsonRpcRequest,
) -> Option<Result<Value, (i64, &'static str)>> {
    match request.method.as_str() {
        "notifications/initialized" | "notifications/cancelled" => None,
        "initialize" => Some(Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "distill", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Use distill_read and distill_run for explicit projected acquisition. This server does not intercept Claude native Read or Bash.",
        }))),
        "ping" => Some(Ok(json!({}))),
        "tools/list" => Some(Ok(json!({"tools": tool_definitions()}))),
        "tools/call" => Some(call_tool(engine, request.id.as_ref(), request.params)),
        _ => Some(Err((-32601, "method not found"))),
    }
}

fn call_tool(
    engine: &Engine,
    id: Option<&Value>,
    params: Value,
) -> Result<Value, (i64, &'static str)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "tools/call requires a tool name"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let request_id = format_request_id(id);
    let handled = match name {
        "distill_read" => {
            let arguments: ReadArguments = serde_json::from_value(arguments)
                .map_err(|_| (-32602, "invalid distill_read arguments"))?;
            let budget = arguments.budget.clone();
            let request = Request {
                contract_version: CONTRACT_VERSION.to_owned(),
                request_id,
                source: Source::File {
                    root_id: arguments.root_id,
                    relative_path: ByteString::from_utf8(arguments.path),
                    binary_policy: arguments.binary_policy.unwrap_or(BinaryPolicy::Accept),
                },
                budget: budget.engine_budget(),
                preservation_profile: "plain-text/v1".to_owned(),
                retention: Retention::default(),
            };
            engine
                .handle(request)
                .and_then(|outcome| render_outcome(outcome, &budget))
        }
        "distill_run" => {
            let arguments: RunArguments = serde_json::from_value(arguments)
                .map_err(|_| (-32602, "invalid distill_run arguments"))?;
            let budget = arguments.budget.clone();
            let request = Request {
                contract_version: CONTRACT_VERSION.to_owned(),
                request_id,
                source: Source::Process {
                    executable: ByteString::from_utf8(arguments.executable),
                    argv: arguments
                        .argv
                        .into_iter()
                        .map(ByteString::from_utf8)
                        .collect(),
                    cwd_root_id: arguments.cwd_root_id,
                    cwd_relative_path: ByteString::from_utf8(normalize_root_relative(
                        arguments.cwd,
                    )),
                    timeout_ms: arguments.timeout_ms,
                    environment_profile: arguments.environment_profile,
                },
                budget: budget.engine_budget(),
                preservation_profile: "plain-text/v1".to_owned(),
                retention: Retention::default(),
            };
            engine
                .handle(request)
                .and_then(|outcome| render_outcome(outcome, &budget))
        }
        _ => {
            return Ok(tool_error(
                "unsupported_capability",
                "Distill v1 exposes exactly distill_read and distill_run",
                None,
            ));
        }
    };
    Ok(match handled {
        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
        Err(failure) => tool_error(
            failure.code.as_str(),
            &failure.safe_message,
            failure.artifact.as_ref(),
        ),
    })
}

fn render_outcome(outcome: Outcome, budget: &McpBudget) -> Result<String, Failure> {
    let process = outcome.receipt.acquisition.process.as_ref().map(|process| {
        json!({
            "exit_code": process.exit_code,
            "signal": process.signal,
            "timed_out": process.timed_out,
            "working_directory": process.working_directory,
        })
    });
    let envelope = serde_json::to_string(&json!({
        "schema_version": MCP_ADAPTER_VERSION,
        "source_is_untrusted": true,
        "projection": outcome.visible.bytes,
        "artifact": outcome.artifact,
        "fidelity": outcome.receipt.fidelity,
        "accounting": {
            "original": outcome.receipt.original_count,
            "visible": outcome.receipt.visible_count,
            "unit": outcome.receipt.count_unit,
            "token_profile": outcome.receipt.token_profile,
        },
        "receipt": {
            "schema_version": outcome.receipt.schema_version,
            "source_sha256": outcome.receipt.source_sha256,
            "projection_version": outcome.receipt.projection_version,
            "policy_version": outcome.receipt.policy_version,
        },
        "source": {
            "variant": outcome.receipt.acquisition.variant,
            "complete": outcome.receipt.acquisition.complete,
            "partial": outcome.receipt.acquisition.partial,
            "truncated": outcome.receipt.acquisition.truncated,
            "root_id": outcome.receipt.acquisition.root_id,
            "relative_path": outcome.receipt.acquisition.relative_path,
            "process": process,
        },
        "recovery": format!("distill artifact get {}", outcome.artifact.id),
    }))
    .map_err(|_| {
        adapter_failure(
            FailureCode::InvariantBreach,
            "MCP outcome envelope cannot be serialized",
            None,
        )
    })?;
    if count(&envelope, budget) > budget.total_visible_limit {
        return Err(adapter_failure(
            FailureCode::BudgetUnsatisfiable,
            "MCP adapter envelope exceeds the declared total visible budget",
            Some(outcome.artifact),
        ));
    }
    Ok(envelope)
}

fn adapter_failure(
    code: FailureCode,
    message: &str,
    artifact: Option<distill::ArtifactRef>,
) -> Failure {
    Failure {
        code,
        safe_message: message.to_owned(),
        request_id: None,
        details: BTreeMap::new(),
        artifact,
        acquisition: None,
    }
}

fn count(text: &str, budget: &McpBudget) -> u64 {
    match budget.unit {
        CountUnit::Bytes => text.len() as u64,
        CountUnit::Tokens => cl100k_base_singleton().encode_ordinary(text).len() as u64,
    }
}

fn tool_error(code: &str, message: &str, artifact: Option<&distill::ArtifactRef>) -> Value {
    let text = serde_json::to_string(&json!({
        "schema_version": MCP_ADAPTER_VERSION,
        "error": {
            "code": code,
            "message": message.chars().take(512).collect::<String>(),
            "artifact": artifact,
        },
        "raw_content_included": false,
        "recovery": artifact.map(|value| format!("distill artifact get {}", value.id)),
    }))
    .unwrap_or_else(|_| "{\"error\":{\"code\":\"invariant_breach\"}}".to_owned());
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": true,
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "distill_read",
            "description": "Explicitly read and project a file through Distill before content reaches the model. This replaces an explicit Claude Read choice only; it does not intercept native Read.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["root_id", "path", "budget"],
                "properties": {
                    "root_id": {"type": "string", "maxLength": MAX_IDENTIFIER_BYTES, "description": "Configured acquisition root ID."},
                    "path": {"type": "string", "maxLength": MAX_PATH_BYTES, "description": "Path relative to the configured root."},
                    "budget": budget_schema(),
                    "binary_policy": {"type": "string", "enum": ["accept", "reject"], "description": "Whether binary bytes may be captured."}
                }
            },
            "annotations": {
                "title": "Distill projected file read",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false
            }
        }),
        json!({
            "name": "distill_run",
            "description": "Explicitly execute one non-interactive executable with argv and project its output through Distill. This replaces an explicit Claude Bash choice only; it does not intercept native Bash and never invokes a shell implicitly.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["executable", "argv", "cwd_root_id", "cwd", "budget"],
                "properties": {
                    "executable": {"type": "string", "maxLength": MAX_PROCESS_EXECUTABLE_BYTES, "description": format!("Executable path or name. A shell runs only when explicitly supplied here. Executable plus argv is capped at {MAX_PROCESS_ARGUMENT_BYTES} UTF-8 bytes.")},
                    "argv": {"type": "array", "maxItems": MAX_PROCESS_ARGUMENTS, "items": {"type": "string"}, "description": "Literal argument vector without shell parsing."},
                    "cwd_root_id": {"type": "string", "maxLength": MAX_IDENTIFIER_BYTES, "description": "Configured root containing the working directory."},
                    "cwd": {"type": "string", "maxLength": MAX_PATH_BYTES, "description": "Working directory relative to cwd_root_id."},
                    "budget": budget_schema(),
                    "timeout_ms": {"type": "integer", "minimum": MIN_PROCESS_TIMEOUT_MS, "maximum": MAX_PROCESS_TIMEOUT_MS, "description": "Non-interactive process timeout in milliseconds."},
                    "environment_profile": {"type": "string", "maxLength": MAX_IDENTIFIER_BYTES, "description": "Optional preconfigured environment allowlist profile."}
                }
            },
            "annotations": {
                "title": "Distill projected process run",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false
            }
        }),
    ]
}

fn budget_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["unit", "total_visible_limit"],
        "properties": {
            "unit": {"type": "string", "enum": ["bytes", "tokens"], "description": "Versioned budget count unit."},
            "total_visible_limit": {"type": "integer", "minimum": 1, "description": "Total model-visible limit including the adapter envelope."},
            "reserved_envelope": {"type": "integer", "minimum": 0, "description": "Optional explicit adapter-envelope allowance."}
        }
    })
}

fn normalize_root_relative(path: String) -> String {
    if path == "." { String::new() } else { path }
}

fn format_request_id(id: Option<&Value>) -> String {
    let suffix = id
        .map(Value::to_string)
        .unwrap_or_else(|| "notification".to_owned());
    let mut request_id = format!("mcp:{suffix}");
    if request_id.len() > 128 {
        request_id.truncate(128);
    }
    request_id
}

fn write_result<W: Write>(output: &mut W, id: Value, result: Value) -> Result<(), SurfaceError> {
    write_message(
        output,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

fn write_protocol_error<W: Write>(
    output: &mut W,
    id: Value,
    code: i64,
    message: &str,
) -> Result<(), SurfaceError> {
    write_message(
        output,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        }),
    )
}

fn write_message<W: Write>(output: &mut W, message: &Value) -> Result<(), SurfaceError> {
    serde_json::to_writer(&mut *output, message)
        .map_err(|error| SurfaceError::output(std::io::Error::other(error)))?;
    output.write_all(b"\n").map_err(SurfaceError::output)
}

fn drain_line<R: BufRead>(reader: &mut R) -> Result<(), SurfaceError> {
    loop {
        let (consumed, finished) = {
            let available = reader
                .fill_buf()
                .map_err(|_| SurfaceError::invalid("cannot drain oversized MCP message"))?;
            if available.is_empty() {
                return Ok(());
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(position) => (position + 1, true),
                None => (available.len(), false),
            }
        };
        reader.consume(consumed);
        if finished {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn exchange(config: EngineConfig, messages: &[Value]) -> Vec<Value> {
        let mut input = Vec::new();
        for message in messages {
            serde_json::to_writer(&mut input, message).expect("message");
            input.push(b'\n');
        }
        let mut output = Vec::new();
        run(config, &mut input.as_slice(), &mut output, &mut Vec::new()).expect("MCP run");
        String::from_utf8(output)
            .expect("UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("response"))
            .collect()
    }

    fn config(temp: &TempDir) -> EngineConfig {
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let mut config = EngineConfig::local(temp.path().join("store/artifacts.db"));
        config.roots.insert("workspace".to_owned(), workspace);
        config
    }

    #[test]
    fn initialize_and_list_expose_exactly_two_explicit_tools() {
        let temp = TempDir::new().expect("temp");
        let responses = exchange(
            config(&temp),
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":MCP_PROTOCOL_VERSION}}),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            ],
        );
        assert_eq!(responses.len(), 2);
        assert_eq!(
            responses[0]["result"]["protocolVersion"],
            MCP_PROTOCOL_VERSION
        );
        let tools = responses[1]["result"]["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "distill_read");
        assert_eq!(tools[1]["name"], "distill_run");
        assert_eq!(
            tools[1]["inputSchema"]["properties"]["argv"]["maxItems"],
            MAX_PROCESS_ARGUMENTS
        );
        assert_eq!(
            tools[1]["inputSchema"]["properties"]["timeout_ms"]["minimum"],
            MIN_PROCESS_TIMEOUT_MS
        );
        assert_eq!(
            tools[1]["inputSchema"]["properties"]["timeout_ms"]["maximum"],
            MAX_PROCESS_TIMEOUT_MS
        );
        assert!(
            tools[1]["inputSchema"]["properties"]["executable"]["description"]
                .as_str()
                .expect("executable description")
                .contains(&MAX_PROCESS_ARGUMENT_BYTES.to_string())
        );
        assert!(
            tools[0]["description"]
                .as_str()
                .expect("description")
                .contains("does not intercept")
        );
        assert!(
            tools[1]["inputSchema"]["properties"]
                .get("preservation_profile")
                .is_none()
        );
        assert!(
            tools[0]["inputSchema"]["properties"]
                .get("syntax_hint")
                .is_none()
        );
        assert!(responses[1]["result"].get("structuredContent").is_none());
    }

    #[test]
    fn removed_syntax_hint_is_rejected_instead_of_ignored() {
        let temp = TempDir::new().expect("temp");
        let responses = exchange(
            config(&temp),
            &[json!({
                "jsonrpc":"2.0","id":"read","method":"tools/call",
                "params":{"name":"distill_read","arguments":{
                    "root_id":"workspace",
                    "path":"source.rs",
                    "syntax_hint":"rust",
                    "budget":{"unit":"bytes","total_visible_limit":1400}
                }}
            })],
        );
        assert_eq!(responses[0]["error"]["code"], -32602);
        assert_eq!(
            responses[0]["error"]["message"],
            "invalid distill_read arguments"
        );
    }

    #[test]
    fn run_argv_is_required_by_schema_and_runtime_decoder() {
        let temp = TempDir::new().expect("temp");
        let responses = exchange(
            config(&temp),
            &[
                json!({"jsonrpc":"2.0","id":"schema","method":"tools/list"}),
                json!({
                    "jsonrpc":"2.0","id":"run","method":"tools/call",
                    "params":{"name":"distill_run","arguments":{
                        "executable":"/usr/bin/true",
                        "cwd_root_id":"workspace",
                        "cwd":".",
                        "budget":{"unit":"bytes","total_visible_limit":1400}
                    }}
                }),
            ],
        );
        let required = responses[0]["result"]["tools"][1]["inputSchema"]["required"]
            .as_array()
            .expect("required fields");
        assert!(required.contains(&json!("argv")));
        assert_eq!(responses[1]["error"]["code"], -32602);
        assert_eq!(
            responses[1]["error"]["message"],
            "invalid distill_run arguments"
        );
    }

    #[test]
    fn notifications_never_receive_null_id_responses() {
        let temp = TempDir::new().expect("temp");
        let responses = exchange(
            config(&temp),
            &[
                json!({"jsonrpc":"2.0","method":"unknown-notification"}),
                json!({"jsonrpc":"2.0","method":"tools/list"}),
                json!({"jsonrpc":"2.0","id":true,"method":"ping"}),
                json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
            ],
        );
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["error"]["code"], -32600);
        assert_eq!(responses[0]["id"], Value::Null);
        assert_eq!(responses[1]["id"], 1);
    }

    #[test]
    fn read_and_run_return_bounded_projection_and_source_metadata() {
        let temp = TempDir::new().expect("temp");
        let config = config(&temp);
        let workspace = config.roots.get("workspace").expect("root");
        fs::write(
            workspace.join("large.txt"),
            format!("first\n{}\nlast\n", "middle secret ".repeat(2_000)),
        )
        .expect("file");
        let calls = [
            json!({
                "jsonrpc":"2.0","id":"read","method":"tools/call",
                "params":{"name":"distill_read","arguments":{
                    "root_id":"workspace","path":"large.txt",
                    "budget":{"unit":"bytes","total_visible_limit":1400}
                }}
            }),
            json!({
                "jsonrpc":"2.0","id":"run","method":"tools/call",
                "params":{"name":"distill_run","arguments":{
                    "executable":"/usr/bin/printf","argv":["%s","literal;$(no-shell)"],
                    "cwd_root_id":"workspace","cwd":".",
                    "budget":{
                        "unit":"tokens",
                        "total_visible_limit":1400,
                        "reserved_envelope":400
                    }
                }}
            }),
        ];
        let responses = exchange(config, &calls);
        for response in &responses {
            assert!(response["result"]["content"][0]["text"].is_string());
            assert!(response["result"].get("structuredContent").is_none());
            let text = response["result"]["content"][0]["text"]
                .as_str()
                .expect("text");
            assert!(text.len() <= 1400);
            let envelope: Value = serde_json::from_str(text).expect("envelope");
            assert_eq!(envelope["schema_version"], MCP_ADAPTER_VERSION);
            assert!(envelope["artifact"].is_object());
            assert!(envelope["accounting"].is_object());
            assert!(envelope["accounting"].get("token_profile").is_some());
            assert!(envelope["receipt"].is_object());
            assert!(envelope["source"].is_object());
        }
        let read_text = responses[0]["result"]["content"][0]["text"]
            .as_str()
            .expect("read text");
        assert!(!read_text.contains("middle secret middle secret"));
        let run_text = responses[1]["result"]["content"][0]["text"]
            .as_str()
            .expect("run text");
        assert!(run_text.contains("literal;$(no-shell)"));
    }

    #[test]
    fn application_and_protocol_errors_are_bounded_without_raw_content() {
        let temp = TempDir::new().expect("temp");
        let responses = exchange(
            config(&temp),
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"third_tool","arguments":{}}}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"distill_read","arguments":{"root_id":"missing","path":"secret","budget":{"unit":"bytes","total_visible_limit":1024}}}}),
                json!({"jsonrpc":"2.0","id":3,"method":"unknown"}),
            ],
        );
        assert_eq!(responses[0]["result"]["isError"], true);
        assert!(
            responses[0]["result"]["content"][0]["text"]
                .as_str()
                .expect("error")
                .contains("unsupported_capability")
        );
        let unsafe_error = responses[1]["result"]["content"][0]["text"]
            .as_str()
            .expect("unsafe error");
        assert!(unsafe_error.contains("unsafe_root"));
        assert!(!unsafe_error.contains("secret"));
        assert_eq!(responses[2]["error"]["code"], -32601);
    }

    #[test]
    fn malformed_json_gets_protocol_error_and_server_continues() {
        let temp = TempDir::new().expect("temp");
        let mut input = b"{\n".to_vec();
        serde_json::to_writer(&mut input, &json!({"jsonrpc":"2.0","id":2,"method":"ping"}))
            .expect("ping");
        input.push(b'\n');
        let mut output = Vec::new();
        run(
            config(&temp),
            &mut input.as_slice(),
            &mut output,
            &mut Vec::new(),
        )
        .expect("run");
        let lines: Vec<_> = String::from_utf8(output)
            .expect("UTF-8")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSON"))
            .collect();
        assert_eq!(lines[0]["error"]["code"], -32700);
        assert_eq!(lines[1]["result"], json!({}));
    }

    #[test]
    fn oversized_frames_resynchronize_without_consuming_the_next_request() {
        fn initialize(id: &str) -> Vec<u8> {
            let mut line = serde_json::to_vec(
                &json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{}}),
            )
            .expect("initialize");
            line.push(b'\n');
            line
        }

        for mut oversized in [
            {
                let mut frame = vec![b'x'; MAX_MESSAGE_BYTES];
                frame.push(b'\n');
                frame
            },
            {
                let mut frame = vec![b'x'; MAX_MESSAGE_BYTES + 1];
                frame.push(b'\n');
                frame
            },
        ] {
            let temp = TempDir::new().expect("temp");
            oversized.extend(initialize("next"));
            let mut output = Vec::new();
            let mut diagnostics = Vec::new();
            run(
                config(&temp),
                &mut oversized.as_slice(),
                &mut output,
                &mut diagnostics,
            )
            .expect("MCP run");
            let responses = String::from_utf8(output)
                .expect("UTF-8")
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).expect("JSON-RPC"))
                .collect::<Vec<_>>();
            assert_eq!(responses.len(), 2);
            assert_eq!(responses[0]["error"]["code"], -32600);
            assert_eq!(responses[1]["id"], "next");
            assert!(responses[1]["result"]["serverInfo"].is_object());
            assert!(diagnostics.is_empty());
        }

        let temp = TempDir::new().expect("temp");
        let unterminated = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        let mut output = Vec::new();
        let mut diagnostics = Vec::new();
        run(
            config(&temp),
            &mut unterminated.as_slice(),
            &mut output,
            &mut diagnostics,
        )
        .expect("MCP EOF");
        let responses = String::from_utf8(output)
            .expect("UTF-8")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSON-RPC"))
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["error"]["code"], -32600);
        assert!(diagnostics.is_empty());
    }
}
