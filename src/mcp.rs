use crate::surface::{
    Recovery, SurfaceError, adapter_failure, bounded_correlation_id, budget_for, count_visible,
    default_request, mcp_error_envelope, normalize_root_relative, projection_envelope,
};
use distill::{
    ArtifactSelector, BinaryPolicy, Budget, ByteString, CountUnit, Engine, EngineConfig, Failure,
    FailureCode, MAX_IDENTIFIER_BYTES, MAX_PATH_BYTES, MAX_PROCESS_ARGUMENT_BYTES,
    MAX_PROCESS_ARGUMENTS, MAX_PROCESS_EXECUTABLE_BYTES, MAX_PROCESS_TIMEOUT_MS,
    MAX_SELECTOR_CONTEXT_LINES, MAX_SELECTOR_MATCHES, MAX_SELECTOR_PATTERN_BYTES,
    MIN_PROCESS_TIMEOUT_MS, Outcome, Source,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};

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

#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
}

#[derive(Clone, Copy, Debug)]
struct ProtocolError {
    code: i64,
    message: &'static str,
}

impl ProtocolError {
    const fn new(code: i64, message: &'static str) -> Self {
        Self { code, message }
    }
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
        budget_for(
            self.unit,
            self.total_visible_limit,
            self.reserved_envelope.unwrap_or(default_reserve),
        )
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
struct SliceArguments {
    artifact_id: String,
    start_line: u64,
    line_count: u64,
    budget: McpBudget,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    artifact_id: String,
    pattern: String,
    budget: McpBudget,
    #[serde(default)]
    before_lines: Option<u64>,
    #[serde(default)]
    after_lines: Option<u64>,
    #[serde(default)]
    max_matches: Option<u64>,
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
        let mut request: JsonRpcRequest = match serde_json::from_slice(&line) {
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
        if matches!(
            request.method.as_str(),
            "notifications/initialized" | "notifications/cancelled"
        ) {
            continue;
        }
        let Some(id) = request.id.take() else {
            continue;
        };
        match dispatch(&engine, &id, &request.method, request.params) {
            Ok(result) => write_result(output, id, result)?,
            Err(error) => {
                write_protocol_error(output, id, error.code, error.message)?;
            }
        }
    }
}

fn dispatch(
    engine: &Engine,
    id: &Value,
    method: &str,
    params: Value,
) -> Result<Value, ProtocolError> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "distill", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Use distill_read and distill_run for explicit projected acquisition, then distill_artifact_slice or distill_artifact_search to recover a region an earlier projection omitted. This server does not intercept Claude native Read or Bash.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => call_tool(engine, id, params),
        _ => Err(ProtocolError::new(-32601, "method not found")),
    }
}

fn call_tool(engine: &Engine, id: &Value, params: Value) -> Result<Value, ProtocolError> {
    let params: ToolCallParams = serde_json::from_value(params)
        .map_err(|_| ProtocolError::new(-32602, "tools/call requires a tool name"))?;
    let name = params.name;
    let arguments = params.arguments;
    let request_id = format_request_id(id);
    let handled = match name.as_str() {
        "distill_read" => {
            let arguments: ReadArguments = serde_json::from_value(arguments)
                .map_err(|_| ProtocolError::new(-32602, "invalid distill_read arguments"))?;
            let budget = arguments.budget.clone();
            let request = default_request(
                request_id,
                Source::File {
                    root_id: arguments.root_id,
                    relative_path: ByteString::from_utf8(arguments.path),
                    binary_policy: arguments.binary_policy.unwrap_or(BinaryPolicy::Accept),
                },
                budget.engine_budget(),
            );
            engine
                .handle(request)
                .and_then(|outcome| render_outcome(outcome, &budget))
        }
        "distill_run" => {
            let arguments: RunArguments = serde_json::from_value(arguments)
                .map_err(|_| ProtocolError::new(-32602, "invalid distill_run arguments"))?;
            let budget = arguments.budget.clone();
            let request = default_request(
                request_id,
                Source::Process {
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
                budget.engine_budget(),
            );
            engine
                .handle(request)
                .and_then(|outcome| render_outcome(outcome, &budget))
        }
        "distill_artifact_slice" => serde_json::from_value::<SliceArguments>(arguments)
            .map_err(|_| invalid_arguments("distill_artifact_slice"))
            .and_then(|arguments| {
                retrieve(
                    engine,
                    request_id,
                    &arguments.artifact_id,
                    ArtifactSelector::Lines {
                        start_line: arguments.start_line,
                        line_count: arguments.line_count,
                    },
                    &arguments.budget,
                )
            }),
        "distill_artifact_search" => serde_json::from_value::<SearchArguments>(arguments)
            .map_err(|_| invalid_arguments("distill_artifact_search"))
            .and_then(|arguments| {
                retrieve(
                    engine,
                    request_id,
                    &arguments.artifact_id,
                    ArtifactSelector::Pattern {
                        pattern: ByteString::from_utf8(arguments.pattern),
                        before_lines: arguments.before_lines,
                        after_lines: arguments.after_lines,
                        max_matches: arguments.max_matches,
                    },
                    &arguments.budget,
                )
            }),
        _ => {
            return Ok(tool_error(
                "unsupported_capability",
                "Distill exposes exactly distill_read, distill_run, distill_artifact_slice, and distill_artifact_search",
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

/// Bounded retrieval over an artifact this store already committed. It resolves
/// the reference, then reuses `Engine::handle` rather than adding a second
/// policy path, so every typed failure reaches the caller unchanged.
fn retrieve(
    engine: &Engine,
    request_id: String,
    artifact_id: &str,
    selector: ArtifactSelector,
    budget: &McpBudget,
) -> Result<String, Failure> {
    distill::validate_artifact_selector(&selector)?;
    let artifact = engine.resolve_artifact(artifact_id)?;
    let request = default_request(
        request_id,
        Source::Artifact {
            artifact,
            selector: Some(selector),
        },
        budget.engine_budget(),
    );
    engine
        .handle(request)
        .and_then(|outcome| render_outcome(outcome, budget))
}

fn invalid_arguments(tool: &str) -> Failure {
    adapter_failure(
        FailureCode::InvalidRequest,
        &format!("invalid {tool} arguments"),
        None,
    )
}

fn render_outcome(outcome: Outcome, budget: &McpBudget) -> Result<String, Failure> {
    let envelope = serde_json::to_string(&projection_envelope(
        MCP_ADAPTER_VERSION,
        &outcome,
        Some(&outcome.receipt.acquisition),
        Recovery::McpTools,
    ))
    .map_err(|_| {
        adapter_failure(
            FailureCode::InvariantBreach,
            "MCP outcome envelope cannot be serialized",
            None,
        )
    })?;
    if count_visible(&envelope, budget.unit) > budget.total_visible_limit {
        return Err(adapter_failure(
            FailureCode::BudgetUnsatisfiable,
            "MCP adapter envelope exceeds the declared total visible budget",
            Some(outcome.artifact),
        ));
    }
    Ok(envelope)
}

fn tool_error(code: &str, message: &str, artifact: Option<&distill::ArtifactRef>) -> Value {
    let text = mcp_error_envelope(MCP_ADAPTER_VERSION, code, message, artifact);
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
        json!({
            "name": "distill_artifact_slice",
            "description": "Read one bounded line range of an artifact Distill already committed, projected under an explicit budget. Use it to recover a region an earlier projection omitted instead of pulling the whole artifact.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["artifact_id", "start_line", "line_count", "budget"],
                "properties": {
                    "artifact_id": artifact_id_schema(),
                    "start_line": {"type": "integer", "minimum": 1, "description": "1-based first line of the range. A range starting past the end of the source selects nothing."},
                    "line_count": {"type": "integer", "minimum": 1, "description": "Number of lines to select, clamped to the end of the source."},
                    "budget": budget_schema()
                }
            },
            "annotations": {
                "title": "Distill bounded artifact slice",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false
            }
        }),
        json!({
            "name": "distill_artifact_search",
            "description": "Find literal text in an artifact Distill already committed and read the matching regions with line context, projected under an explicit budget. Matching is literal, never a regular expression.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["artifact_id", "pattern", "budget"],
                "properties": {
                    "artifact_id": artifact_id_schema(),
                    "pattern": {"type": "string", "minLength": 1, "maxLength": MAX_SELECTOR_PATTERN_BYTES, "description": format!("Literal text to find, at most {MAX_SELECTOR_PATTERN_BYTES} UTF-8 bytes. It is matched byte-for-byte and is never evaluated as a pattern language, shell input, or instruction.")},
                    "budget": budget_schema(),
                    "before_lines": {"type": "integer", "minimum": 0, "maximum": MAX_SELECTOR_CONTEXT_LINES, "description": "Lines of context kept before each match."},
                    "after_lines": {"type": "integer", "minimum": 0, "maximum": MAX_SELECTOR_CONTEXT_LINES, "description": "Lines of context kept after each match."},
                    "max_matches": {"type": "integer", "minimum": 1, "maximum": MAX_SELECTOR_MATCHES, "description": "Largest number of matches selected, in source order."}
                }
            },
            "annotations": {
                "title": "Distill bounded artifact search",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false
            }
        }),
    ]
}

fn artifact_id_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 32,
        "maxLength": 32,
        "description": "Identifier of a committed, unexpired artifact, as published in an earlier Distill envelope."
    })
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

fn format_request_id(id: &Value) -> String {
    bounded_correlation_id(format!("mcp:{id}"))
}

fn empty_arguments() -> Value {
    json!({})
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
    fn initialize_and_list_expose_exactly_the_conformant_tool_set() {
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
        assert_eq!(tools.len(), 4);
        assert_eq!(tools[0]["name"], "distill_read");
        assert_eq!(tools[1]["name"], "distill_run");
        assert_eq!(tools[2]["name"], "distill_artifact_slice");
        assert_eq!(tools[3]["name"], "distill_artifact_search");
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

    fn large_artifact(config: EngineConfig, name: &str, body: &str) -> (Vec<Value>, String) {
        let workspace = config.roots.get("workspace").expect("root").clone();
        fs::write(workspace.join(name), body).expect("file");
        let responses = exchange(
            config,
            &[json!({
                "jsonrpc":"2.0","id":"read","method":"tools/call",
                "params":{"name":"distill_read","arguments":{
                    "root_id":"workspace","path":name,
                    "budget":{"unit":"bytes","total_visible_limit":1400}
                }}
            })],
        );
        let envelope: Value = serde_json::from_str(
            responses[0]["result"]["content"][0]["text"]
                .as_str()
                .expect("read text"),
        )
        .expect("read envelope");
        let id = envelope["artifact"]["id"]
            .as_str()
            .expect("artifact ID")
            .to_owned();
        (responses, id)
    }

    fn tool_text(response: &Value) -> Value {
        serde_json::from_str(
            response["result"]["content"][0]["text"]
                .as_str()
                .expect("tool text"),
        )
        .expect("tool envelope")
    }

    /// US-008: the published retrieval schemas, the runtime decoder, and the
    /// versioned conformance matrix pin the same required fields and limits.
    #[test]
    fn published_retrieval_schemas_match_the_decoder_and_the_matrix() {
        let matrix: Value =
            serde_json::from_str(include_str!("../docs/integrations/mcp-conformance-v1.json"))
                .expect("MCP conformance matrix");
        assert_eq!(matrix["schema_version"], "distill.mcp-conformance/v1");
        assert_eq!(matrix["surface_schema_version"], MCP_ADAPTER_VERSION);
        assert_eq!(matrix["protocol_version"], MCP_PROTOCOL_VERSION);
        assert_eq!(
            matrix["request_contract_version"],
            distill::CONTRACT_VERSION
        );
        assert_eq!(
            matrix["retrieval"]["unbounded_recovery_is_published"],
            false
        );

        // US-014: the surface declares no content class. The matrix pins the
        // shape-derived default, every retired identifier it resolves, and the
        // receipt fields that record what actually ran.
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
        assert_eq!(preservation["published_profile_argument"], false);

        let temp = TempDir::new().expect("temp");
        let responses = exchange(
            config(&temp),
            &[json!({"jsonrpc":"2.0","id":"schema","method":"tools/list"})],
        );
        let tools = responses[0]["result"]["tools"].as_array().expect("tools");
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool["name"].clone())
                .collect::<Vec<_>>(),
            matrix["tools"].as_array().expect("matrix tools").clone()
        );

        let published = |name: &str| {
            tools
                .iter()
                .find(|tool| tool["name"] == name)
                .expect("published tool")
                .clone()
        };
        for entry in matrix["retrieval"]["tools"]
            .as_array()
            .expect("matrix retrieval tools")
        {
            let tool = published(entry["name"].as_str().expect("matrix tool name"));
            assert_eq!(tool["inputSchema"]["required"], entry["required"]);
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }

        let bounds = &matrix["retrieval"]["selector_bounds"];
        assert_eq!(bounds["max_pattern_bytes"], MAX_SELECTOR_PATTERN_BYTES);
        assert_eq!(bounds["max_context_lines"], MAX_SELECTOR_CONTEXT_LINES);
        assert_eq!(bounds["max_matches"], MAX_SELECTOR_MATCHES);
        assert_eq!(
            bounds["default_context_lines"],
            distill::DEFAULT_SELECTOR_CONTEXT_LINES
        );
        assert_eq!(bounds["default_matches"], distill::DEFAULT_SELECTOR_MATCHES);

        let search = published("distill_artifact_search");
        let properties = &search["inputSchema"]["properties"];
        assert_eq!(
            properties["pattern"]["maxLength"],
            bounds["max_pattern_bytes"]
        );
        assert_eq!(
            properties["before_lines"]["maximum"],
            bounds["max_context_lines"]
        );
        assert_eq!(
            properties["after_lines"]["maximum"],
            bounds["max_context_lines"]
        );
        assert_eq!(properties["max_matches"]["maximum"], bounds["max_matches"]);
        assert_eq!(
            properties["artifact_id"]["maxLength"],
            bounds["artifact_id_length"]
        );
        assert!(
            properties["pattern"]["description"]
                .as_str()
                .expect("pattern description")
                .contains("never evaluated")
        );

        let slice = published("distill_artifact_slice");
        let properties = &slice["inputSchema"]["properties"];
        assert_eq!(
            properties["start_line"]["minimum"],
            bounds["min_start_line"]
        );
        assert_eq!(
            properties["line_count"]["minimum"],
            bounds["min_line_count"]
        );
    }

    /// US-008 and US-009: retrieval recovers an omitted region on the agent's
    /// own surface, inside the declared budget, and the omitting envelope names
    /// the bounded tools rather than an unbounded command.
    #[test]
    fn bounded_retrieval_recovers_an_omitted_region_within_its_budget() {
        let temp = TempDir::new().expect("temp");
        let body = (0..600)
            .map(|index| format!("line {index:03} DISTILL_MARKER_{index:03}\n"))
            .collect::<String>();
        let (read, id) = large_artifact(config(&temp), "large.txt", &body);

        let projection = tool_text(&read[0]);
        assert_eq!(projection["fidelity"], "extractive");
        let recovery = projection["recovery"].as_str().expect("recovery");
        assert!(recovery.contains("distill_artifact_slice"));
        assert!(recovery.contains("distill_artifact_search"));
        assert!(recovery.contains(&id));
        assert!(!recovery.contains("artifact get"));
        assert!(
            projection["accounting"]["omitted"]
                .as_u64()
                .expect("omitted")
                > 0
        );

        let responses = exchange(
            config(&temp),
            &[
                json!({
                    "jsonrpc":"2.0","id":"slice","method":"tools/call",
                    "params":{"name":"distill_artifact_slice","arguments":{
                        "artifact_id": id,
                        "start_line": 301,
                        "line_count": 3,
                        "budget":{"unit":"bytes","total_visible_limit":1400}
                    }}
                }),
                json!({
                    "jsonrpc":"2.0","id":"search","method":"tools/call",
                    "params":{"name":"distill_artifact_search","arguments":{
                        "artifact_id": id,
                        "pattern": "DISTILL_MARKER_417",
                        "before_lines": 0,
                        "after_lines": 0,
                        "budget":{"unit":"bytes","total_visible_limit":1400}
                    }}
                }),
            ],
        );

        for response in &responses {
            assert!(response["result"].get("isError").is_none());
            let text = response["result"]["content"][0]["text"]
                .as_str()
                .expect("retrieval text");
            // The envelope respects the declared total visible budget exactly as
            // projection envelopes do, so recovery never costs more than a read.
            assert!(text.len() <= 1400);
            let envelope: Value = serde_json::from_str(text).expect("retrieval envelope");
            assert_eq!(envelope["schema_version"], MCP_ADAPTER_VERSION);
            assert_eq!(envelope["artifact"]["id"], id);
            // The adapter envelope stays inside the allowance it reserved.
            let projection = envelope["projection"].as_str().expect("projection");
            assert!(text.len() - projection.len() <= 1_024);
        }
        assert_eq!(
            tool_text(&responses[0])["projection"],
            "line 300 DISTILL_MARKER_300\nline 301 DISTILL_MARKER_301\nline 302 DISTILL_MARKER_302\n"
        );
        assert_eq!(
            tool_text(&responses[1])["projection"],
            "line 417 DISTILL_MARKER_417\n"
        );
    }

    /// US-008: a retrieval call the decoder rejects, and one naming an artifact
    /// that no longer exists, are bounded tool errors naming the typed failure.
    #[test]
    fn invalid_and_unknown_retrieval_calls_are_bounded_tool_errors() {
        let temp = TempDir::new().expect("temp");
        let config = config(&temp);
        let store = config.store_path.clone();
        let responses = exchange(
            config,
            &[
                json!({
                    "jsonrpc":"2.0","id":"no-budget","method":"tools/call",
                    "params":{"name":"distill_artifact_slice","arguments":{
                        "artifact_id":"0123456789abcdef0123456789abcdef",
                        "start_line":1,"line_count":1
                    }}
                }),
                json!({
                    "jsonrpc":"2.0","id":"no-pattern","method":"tools/call",
                    "params":{"name":"distill_artifact_search","arguments":{
                        "artifact_id":"0123456789abcdef0123456789abcdef",
                        "budget":{"unit":"bytes","total_visible_limit":1400}
                    }}
                }),
                json!({
                    "jsonrpc":"2.0","id":"unknown","method":"tools/call",
                    "params":{"name":"distill_artifact_slice","arguments":{
                        "artifact_id":"0123456789abcdef0123456789abcdef",
                        "start_line":1,"line_count":1,
                        "budget":{"unit":"bytes","total_visible_limit":1400}
                    }}
                }),
                json!({
                    "jsonrpc":"2.0","id":"unbounded-pattern","method":"tools/call",
                    "params":{"name":"distill_artifact_search","arguments":{
                        "artifact_id":"0123456789abcdef0123456789abcdef",
                        "pattern": "x".repeat(MAX_SELECTOR_PATTERN_BYTES + 1),
                        "budget":{"unit":"bytes","total_visible_limit":1400}
                    }}
                }),
            ],
        );

        for (index, expected) in [
            (0, "invalid_request"),
            (1, "invalid_request"),
            (2, "artifact_unknown"),
            (3, "invalid_request"),
        ] {
            assert_eq!(responses[index]["result"]["isError"], true, "{expected}");
            assert!(responses[index].get("error").is_none(), "{expected}");
            let text = responses[index]["result"]["content"][0]["text"]
                .as_str()
                .expect("bounded error");
            assert!(text.contains(expected), "{text}");
            assert!(text.len() <= 1400);
        }
        // The same identifier resolves to `artifact_unknown` only once the
        // arguments decode and the selector holds, which is what proves the
        // rejected calls stopped before any store read.
        assert!(store.exists());
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
