#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rusqlite::{
    Connection, ErrorCode, OpenFlags, TransactionBehavior, params, types::Type as SqlType,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use wait_timeout::ChildExt;

const SCHEMA_VERSION: &str = "distill.spike/v1";
const ARTIFACT_SCHEMA_VERSION: &str = "distill.artifact/v1";
const PROJECTION_VERSION: &str = "extract-lines/v1";
const POLICY_VERSION: &str = "fixture-facts/v1";
const DEFAULT_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;
const DEFAULT_STORE_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_BUSY_TIMEOUT_MS: u64 = 250;
const MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;
const MAX_JSON_LINE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: String,
    request_id: String,
    operation: Operation,
    store_path: PathBuf,
    source: Option<Source>,
    artifact_id: Option<String>,
    budget: Option<Budget>,
    retention: Option<Retention>,
    preservation: Option<Preservation>,
    max_store_bytes: Option<u64>,
    busy_timeout_ms: Option<u64>,
    fault_point: Option<FaultPoint>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Operation {
    Project,
    Recover,
    Run,
    Health,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Source {
    Inline {
        bytes_base64: String,
    },
    Process {
        executable: String,
        argv: Vec<String>,
        cwd: PathBuf,
        timeout_ms: u64,
        output_limit: usize,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Budget {
    unit: CountUnit,
    total_visible_limit: usize,
    reserved_envelope: usize,
    token_profile: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum CountUnit {
    Bytes,
    Tokens,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FaultPoint {
    BeforeInsert,
    BeforeCommit,
    AfterCommit,
    BeforeReadback,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retention {
    ttl_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preservation {
    profile: String,
    p0: Vec<Fact>,
    p1: Vec<Fact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fact {
    id: String,
    needle_base64: String,
}

#[derive(Clone, Debug, Serialize)]
struct ArtifactRef {
    schema_version: &'static str,
    id: String,
    source_sha256: String,
    source_bytes: usize,
    created_at: u64,
    expires_at: u64,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct Projection {
    visible: Vec<u8>,
    fidelity: &'static str,
    retained_spans: Vec<Span>,
    omitted_spans: Vec<Span>,
    preserved_ids: Vec<String>,
}

#[derive(Debug)]
struct Acquired {
    bytes: Vec<u8>,
    receipt: Value,
}

#[derive(Debug)]
struct AppError {
    code: &'static str,
    message: &'static str,
    artifact: Option<ArtifactRef>,
}

impl AppError {
    const fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            artifact: None,
        }
    }

    fn with_artifact(mut self, artifact: ArtifactRef) -> Self {
        self.artifact = Some(artifact);
        self
    }
}

fn main() {
    if let Err(error) = run_cli() {
        let fallback = json!({
            "ok": false,
            "failure": {
                "code": "invariant_breach",
                "message": "failed to write protocol response",
                "details": error.to_string(),
            }
        });
        let _ignored = writeln!(io::stdout(), "{fallback}");
    }
}

fn run_cli() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut reader = BufReader::new(stdin.lock());
    let mut line = Vec::new();

    loop {
        line.clear();
        match read_protocol_line(&mut reader, &mut line)? {
            LineRead::Eof => break,
            LineRead::TooLong => {
                serde_json::to_writer(
                    &mut stdout,
                    &failure_value(
                        None,
                        AppError::new("input_too_large", "JSONL request exceeds 16 MiB"),
                    ),
                )?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
                continue;
            }
            LineRead::Line => {}
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }

        let response = handle_line(&line);
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

enum LineRead {
    Eof,
    Line,
    TooLong,
}

fn read_protocol_line<R: BufRead>(reader: &mut R, line: &mut Vec<u8>) -> io::Result<LineRead> {
    let mut saw_bytes = false;
    let mut too_long = false;
    loop {
        let (consumed, found_newline) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(if !saw_bytes {
                    LineRead::Eof
                } else if too_long {
                    LineRead::TooLong
                } else {
                    LineRead::Line
                });
            }
            saw_bytes = true;
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |position| position + 1);
            let content_len = newline.unwrap_or(consumed);
            if !too_long {
                let remaining = (MAX_JSON_LINE_BYTES + 1).saturating_sub(line.len());
                let copied = remaining.min(content_len);
                line.extend_from_slice(&available[..copied]);
                too_long = copied < content_len || line.len() > MAX_JSON_LINE_BYTES;
            }
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        if found_newline {
            return Ok(if too_long {
                LineRead::TooLong
            } else {
                LineRead::Line
            });
        }
    }
}

fn handle_line(line: &[u8]) -> Value {
    let request: Request = match serde_json::from_slice(line) {
        Ok(request) => request,
        Err(_) => {
            return failure_value(
                None,
                AppError::new("invalid_request", "request is not valid spike JSON"),
            );
        }
    };
    let request_id = request.request_id.clone();
    match handle(request) {
        Ok(outcome) => json!({
            "ok": true,
            "request_id": request_id,
            "outcome": outcome,
        }),
        Err(error) => failure_value(Some(&request_id), error),
    }
}

fn failure_value(request_id: Option<&str>, error: AppError) -> Value {
    json!({
        "ok": false,
        "failure": {
            "code": error.code,
            "message": error.message,
            "request_id": request_id,
            "artifact": error.artifact,
        }
    })
}

fn handle(request: Request) -> Result<Value, AppError> {
    validate_request(&request)?;

    match request.operation {
        Operation::Health => Ok(json!({
            "candidate": "rust",
            "protocol": SCHEMA_VERSION,
            "runtime": env!("CARGO_PKG_RUST_VERSION"),
            "sqlite": rusqlite::version(),
        })),
        Operation::Recover => recover(&request),
        Operation::Project | Operation::Run => project(&request),
    }
}

fn validate_request(request: &Request) -> Result<(), AppError> {
    if request.schema_version != SCHEMA_VERSION {
        return Err(AppError::new(
            "schema_unsupported",
            "unsupported spike schema version",
        ));
    }
    if request.request_id.is_empty() || request.request_id.len() > 128 {
        return Err(AppError::new(
            "invalid_request",
            "request_id must contain 1 to 128 bytes",
        ));
    }
    if request.busy_timeout_ms.unwrap_or(DEFAULT_BUSY_TIMEOUT_MS) > 5_000 {
        return Err(AppError::new(
            "invalid_request",
            "busy_timeout_ms exceeds spike limit",
        ));
    }
    Ok(())
}

fn project(request: &Request) -> Result<Value, AppError> {
    let budget = request
        .budget
        .as_ref()
        .ok_or_else(|| AppError::new("invalid_request", "budget is required"))?;
    if budget.reserved_envelope > budget.total_visible_limit {
        return Err(AppError::new(
            "invalid_request",
            "reserved envelope exceeds total visible limit",
        ));
    }
    if matches!(budget.unit, CountUnit::Tokens) {
        let _profile_was_declared = budget.token_profile.as_deref();
        return Err(AppError::new(
            "token_profile_unsupported",
            "the spike does not implement a versioned tokenizer",
        ));
    }

    let preservation = request
        .preservation
        .as_ref()
        .ok_or_else(|| AppError::new("invalid_request", "preservation profile is required"))?;
    if preservation.profile != POLICY_VERSION {
        return Err(AppError::new(
            "invalid_request",
            "unsupported preservation profile",
        ));
    }
    let facts = decode_facts(preservation)?;
    let acquired = acquire(request)?;
    if acquired.bytes.len() > MAX_INPUT_BYTES {
        return Err(AppError::new(
            "input_too_large",
            "source exceeds the 10 MiB spike limit",
        ));
    }

    let store = Store::open(
        &request.store_path,
        request.busy_timeout_ms.unwrap_or(DEFAULT_BUSY_TIMEOUT_MS),
    )?;
    let artifact = store.capture(
        &acquired.bytes,
        request
            .retention
            .as_ref()
            .map_or(DEFAULT_TTL_SECONDS, |retention| retention.ttl_seconds),
        request.max_store_bytes.unwrap_or(DEFAULT_STORE_BYTES),
        request.fault_point,
    )?;

    let payload_limit = budget.total_visible_limit - budget.reserved_envelope;
    let projection = make_projection(&acquired.bytes, payload_limit, &facts)
        .map_err(|error| error.with_artifact(artifact.clone()))?;
    let visible_base64 = BASE64.encode(&projection.visible);
    let receipt = json!({
        "schema_version": "distill.receipt/v1",
        "request_id": request.request_id,
        "source_sha256": artifact.source_sha256,
        "artifact": artifact,
        "projection_version": PROJECTION_VERSION,
        "policy_version": POLICY_VERSION,
        "token_profile": null,
        "original_count": acquired.bytes.len(),
        "visible_count": projection.visible.len(),
        "count_unit": CountUnit::Bytes,
        "fidelity": projection.fidelity,
        "retained_spans": projection.retained_spans,
        "omitted_spans": projection.omitted_spans,
        "preservation": {
            "profile": POLICY_VERSION,
            "mandatory_fact_ids": projection.preserved_ids,
        },
        "acquisition": acquired.receipt,
    });

    Ok(json!({
        "visible_base64": visible_base64,
        "artifact": artifact,
        "receipt": receipt,
    }))
}

fn recover(request: &Request) -> Result<Value, AppError> {
    let artifact_id = request
        .artifact_id
        .as_deref()
        .ok_or_else(|| AppError::new("invalid_request", "artifact_id is required"))?;
    if artifact_id.len() != 32
        || !artifact_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(AppError::new(
            "invalid_request",
            "artifact_id must be 32 lowercase hex characters",
        ));
    }
    let store = Store::open(
        &request.store_path,
        request.busy_timeout_ms.unwrap_or(DEFAULT_BUSY_TIMEOUT_MS),
    )?;
    let (artifact, bytes) = store.recover(artifact_id)?;
    Ok(json!({
        "artifact": artifact,
        "bytes_base64": BASE64.encode(bytes),
    }))
}

fn acquire(request: &Request) -> Result<Acquired, AppError> {
    let source = request
        .source
        .as_ref()
        .ok_or_else(|| AppError::new("invalid_request", "source is required"))?;
    match (request.operation, source) {
        (Operation::Project, Source::Inline { bytes_base64 }) => {
            let bytes = decode_base64(bytes_base64)?;
            Ok(Acquired {
                bytes,
                receipt: json!({ "variant": "inline" }),
            })
        }
        (
            Operation::Run,
            Source::Process {
                executable,
                argv,
                cwd,
                timeout_ms,
                output_limit,
            },
        ) => acquire_process(executable, argv, cwd, *timeout_ms, *output_limit),
        _ => Err(AppError::new(
            "source_unsupported",
            "operation and source kind do not match",
        )),
    }
}

fn acquire_process(
    executable: &str,
    argv: &[String],
    cwd: &Path,
    timeout_ms: u64,
    output_limit: usize,
) -> Result<Acquired, AppError> {
    if executable.is_empty()
        || argv.len() > 64
        || timeout_ms == 0
        || timeout_ms > 30_000
        || output_limit == 0
        || output_limit > MAX_INPUT_BYTES
    {
        return Err(AppError::new(
            "invalid_request",
            "process source exceeds spike limits",
        ));
    }

    let mut command = Command::new(executable);
    command
        .args(argv)
        .current_dir(cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|_| AppError::new("acquisition_failed", "process spawn failed"))?;
    let process_group = child.id();

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::new("acquisition_failed", "stdout pipe unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::new("acquisition_failed", "stderr pipe unavailable"))?;
    let read_limit = output_limit.saturating_add(1) as u64;
    let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
    let (stderr_sender, stderr_receiver) = mpsc::sync_channel(1);
    let stdout_reader = thread::spawn(move || {
        let _ignored = stdout_sender.send(read_bounded(stdout, read_limit));
    });
    let stderr_reader = thread::spawn(move || {
        let _ignored = stderr_sender.send(read_bounded(stderr, read_limit));
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = match child.wait_timeout(remaining(deadline)) {
        Ok(Some(status)) => status,
        Ok(None) => {
            kill_process_group(process_group);
            let _ignored = child.kill();
            let _ignored = child.wait();
            let _ignored = stdout_reader.join();
            let _ignored = stderr_reader.join();
            return Err(AppError::new(
                "acquisition_failed",
                "process exceeded wall timeout",
            ));
        }
        Err(_) => {
            kill_process_group(process_group);
            let _ignored = child.kill();
            let _ignored = child.wait();
            let _ignored = stdout_reader.join();
            let _ignored = stderr_reader.join();
            return Err(AppError::new("acquisition_failed", "process wait failed"));
        }
    };

    let stdout = match receive_reader(&stdout_receiver, deadline) {
        Ok(bytes) => bytes,
        Err(error) => {
            kill_process_group(process_group);
            let _ignored = stdout_reader.join();
            let _ignored = stderr_reader.join();
            return Err(error);
        }
    };
    let stderr = match receive_reader(&stderr_receiver, deadline) {
        Ok(bytes) => bytes,
        Err(error) => {
            kill_process_group(process_group);
            let _ignored = stdout_reader.join();
            let _ignored = stderr_reader.join();
            return Err(error);
        }
    };
    let _ignored = stdout_reader.join();
    let _ignored = stderr_reader.join();
    if stdout.len().saturating_add(stderr.len()) > output_limit {
        return Err(AppError::new(
            "resource_exhausted",
            "process output exceeds configured limit",
        ));
    }

    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    if signal.is_some() {
        return Err(AppError::new(
            "acquisition_failed",
            "process terminated by signal",
        ));
    }

    let stdout_sha256 = sha256_hex(&stdout);
    let stderr_sha256 = sha256_hex(&stderr);
    let mut bytes = stdout;
    bytes.extend_from_slice(&stderr);
    Ok(Acquired {
        bytes,
        receipt: json!({
            "variant": "process",
            "stdout_sha256": stdout_sha256,
            "stderr_sha256": stderr_sha256,
            "events": [
                { "order": 0, "stream": "stdout" },
                { "order": 1, "stream": "stderr" }
            ],
            "exit_code": status.code(),
            "signal": signal,
            "timed_out": false,
            "working_directory": cwd.to_string_lossy(),
            "truncated": false,
        }),
    })
}

fn read_bounded<R: Read>(reader: R, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn receive_reader(
    receiver: &mpsc::Receiver<io::Result<Vec<u8>>>,
    deadline: Instant,
) -> Result<Vec<u8>, AppError> {
    match receiver.recv_timeout(remaining(deadline)) {
        Ok(Ok(bytes)) => Ok(bytes),
        _ => Err(AppError::new(
            "acquisition_failed",
            "process output collection failed",
        )),
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

#[cfg(unix)]
fn kill_process_group(process_group: u32) {
    if let Ok(group) = i32::try_from(process_group) {
        // The child starts a fresh process group, so a negative PID targets
        // only that acquisition tree and cannot signal the Distill process.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_process_group: u32) {}

fn decode_facts(preservation: &Preservation) -> Result<DecodedFacts, AppError> {
    fn decode_group(facts: &[Fact]) -> Result<Vec<(String, Vec<u8>)>, AppError> {
        facts
            .iter()
            .map(|fact| {
                if fact.id.is_empty() || fact.id.len() > 128 {
                    return Err(AppError::new(
                        "invalid_request",
                        "fact id is outside spike limits",
                    ));
                }
                let needle = decode_base64(&fact.needle_base64)?;
                if needle.is_empty() {
                    return Err(AppError::new(
                        "invalid_request",
                        "fact needle must not be empty",
                    ));
                }
                Ok((fact.id.clone(), needle))
            })
            .collect()
    }
    Ok((
        decode_group(&preservation.p0)?,
        decode_group(&preservation.p1)?,
    ))
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, AppError> {
    let bytes = BASE64
        .decode(encoded)
        .map_err(|_| AppError::new("invalid_request", "invalid base64"))?;
    if BASE64.encode(&bytes) != encoded {
        return Err(AppError::new(
            "invalid_request",
            "base64 must use canonical encoding",
        ));
    }
    Ok(bytes)
}

type DecodedFacts = (Vec<(String, Vec<u8>)>, Vec<(String, Vec<u8>)>);

fn make_projection(
    source: &[u8],
    limit: usize,
    facts: &DecodedFacts,
) -> Result<Projection, AppError> {
    for (_, needle) in &facts.0 {
        if find_bytes(source, needle).is_none() {
            return Err(AppError::new(
                "invariant_breach",
                "mandatory fact is absent from source",
            ));
        }
    }
    if let Ok(text) = std::str::from_utf8(source) {
        if source.len() <= limit {
            return Ok(Projection {
                visible: text.as_bytes().to_vec(),
                fidelity: "exact",
                retained_spans: vec![Span {
                    start: 0,
                    end: source.len(),
                }],
                omitted_spans: Vec::new(),
                preserved_ids: facts.0.iter().map(|(id, _)| id.clone()).collect(),
            });
        }
        return extract_projection(source, limit, facts);
    }

    if !facts.0.is_empty() {
        return Err(AppError::new(
            "budget_unsatisfiable",
            "mandatory facts cannot be proven in encoded content",
        ));
    }
    let visible = format!("base64:{}", BASE64.encode(source)).into_bytes();
    if visible.len() > limit {
        return Err(AppError::new(
            "budget_unsatisfiable",
            "encoded source exceeds the visible budget",
        ));
    }
    Ok(Projection {
        visible,
        fidelity: "encoded",
        retained_spans: Vec::new(),
        omitted_spans: vec![Span {
            start: 0,
            end: source.len(),
        }],
        preserved_ids: Vec::new(),
    })
}

fn extract_projection(
    source: &[u8],
    limit: usize,
    facts: &DecodedFacts,
) -> Result<Projection, AppError> {
    let mut spans = Vec::new();
    let mut preserved_ids = Vec::new();
    for (id, needle) in &facts.0 {
        let position = find_bytes(source, needle).ok_or_else(|| {
            AppError::new("invariant_breach", "mandatory fact is absent from source")
        })?;
        spans.push(line_span(source, position, position + needle.len()));
        preserved_ids.push(id.clone());
    }
    merge_spans(&mut spans);
    if render_spans(source, &spans).len() > limit {
        return Err(AppError::new(
            "budget_unsatisfiable",
            "mandatory facts exceed the visible budget",
        ));
    }

    for (_, needle) in &facts.1 {
        if let Some(position) = find_bytes(source, needle) {
            let mut candidate = spans.clone();
            candidate.push(line_span(source, position, position + needle.len()));
            merge_spans(&mut candidate);
            if render_spans(source, &candidate).len() <= limit {
                spans = candidate;
            }
        }
    }

    if spans.is_empty() {
        let prefix_end = utf8_prefix_len(source, limit);
        if prefix_end > 0 {
            spans.push(Span {
                start: 0,
                end: prefix_end,
            });
        }
    }
    let visible = render_spans(source, &spans);
    if visible.len() > limit {
        return Err(AppError::new(
            "budget_unsatisfiable",
            "projection metadata exceeds the visible budget",
        ));
    }
    let omitted_spans = complement_spans(source.len(), &spans);
    Ok(Projection {
        visible,
        fidelity: "extractive",
        retained_spans: spans,
        omitted_spans,
        preserved_ids,
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn line_span(source: &[u8], start: usize, end: usize) -> Span {
    let line_start = source[..start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let line_end = source[end..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(source.len(), |position| end + position + 1);
    Span {
        start: line_start,
        end: line_end,
    }
}

fn merge_spans(spans: &mut Vec<Span>) {
    spans.sort_by_key(|span| span.start);
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans.iter().copied() {
        if let Some(last) = merged.last_mut()
            && span.start <= last.end
        {
            last.end = last.end.max(span.end);
            continue;
        }
        merged.push(span);
    }
    *spans = merged;
}

fn render_spans(source: &[u8], spans: &[Span]) -> Vec<u8> {
    let mut visible = Vec::new();
    let mut cursor = 0;
    for span in spans {
        if span.start > cursor {
            append_omission(&mut visible, span.start - cursor);
        }
        visible.extend_from_slice(&source[span.start..span.end]);
        cursor = span.end;
    }
    if cursor < source.len() {
        append_omission(&mut visible, source.len() - cursor);
    }
    visible
}

fn append_omission(visible: &mut Vec<u8>, omitted: usize) {
    visible.extend_from_slice(format!("[... omitted {omitted} bytes ...]\n").as_bytes());
}

fn complement_spans(source_len: usize, spans: &[Span]) -> Vec<Span> {
    let mut omitted = Vec::new();
    let mut cursor = 0;
    for span in spans {
        if span.start > cursor {
            omitted.push(Span {
                start: cursor,
                end: span.start,
            });
        }
        cursor = span.end;
    }
    if cursor < source_len {
        omitted.push(Span {
            start: cursor,
            end: source_len,
        });
    }
    omitted
}

fn utf8_prefix_len(source: &[u8], limit: usize) -> usize {
    let mut end = source.len().min(limit);
    while end > 0 && std::str::from_utf8(&source[..end]).is_err() {
        end -= 1;
    }
    end
}

struct Store {
    connection: Connection,
    path: PathBuf,
}

impl Store {
    fn open(path: &Path, busy_timeout_ms: u64) -> Result<Self, AppError> {
        let parent = path.parent().ok_or_else(|| {
            AppError::new("unsafe_root", "store path must have a parent directory")
        })?;
        fs::create_dir_all(parent)
            .map_err(|_| AppError::new("permission_denied", "cannot create store root"))?;
        set_private_dir(parent)?;

        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )
        .map_err(map_store_open_error)?;
        connection
            .busy_timeout(Duration::from_millis(busy_timeout_ms))
            .map_err(map_store_open_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(map_store_open_error)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(map_store_open_error)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS artifacts (
                    id TEXT PRIMARY KEY,
                    source BLOB NOT NULL,
                    sha256 TEXT NOT NULL,
                    source_bytes INTEGER NOT NULL,
                    created_at INTEGER NOT NULL,
                    expires_at INTEGER NOT NULL
                );",
            )
            .map_err(map_store_open_error)?;
        set_private_file(path)?;
        set_private_file(&PathBuf::from(format!("{}-wal", path.display())))?;
        set_private_file(&PathBuf::from(format!("{}-shm", path.display())))?;
        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    fn capture(
        mut self,
        source: &[u8],
        ttl_seconds: u64,
        max_store_bytes: u64,
        fault_point: Option<FaultPoint>,
    ) -> Result<ArtifactRef, AppError> {
        if ttl_seconds == 0 || ttl_seconds > 30 * 24 * 60 * 60 {
            return Err(AppError::new(
                "invalid_request",
                "retention TTL is outside spike limits",
            ));
        }
        let digest = sha256_hex(source);
        let now = unix_seconds()?;
        let expires_at = now.saturating_add(ttl_seconds);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_transaction_error)?;
        let used: i64 = transaction
            .query_row(
                "SELECT COALESCE(SUM(source_bytes), 0) FROM artifacts",
                [],
                |row| row.get(0),
            )
            .map_err(map_transaction_error)?;
        if used < 0 || (used as u64).saturating_add(source.len() as u64) > max_store_bytes {
            return Err(AppError::new(
                "store_full",
                "artifact store byte cap would be exceeded",
            ));
        }
        let id: String = transaction
            .query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))
            .map_err(map_transaction_error)?;
        maybe_fault(fault_point, FaultPoint::BeforeInsert);
        transaction
            .execute(
                "INSERT INTO artifacts
                 (id, source, sha256, source_bytes, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    source,
                    digest,
                    source.len() as i64,
                    now as i64,
                    expires_at as i64
                ],
            )
            .map_err(map_transaction_error)?;
        maybe_fault(fault_point, FaultPoint::BeforeCommit);
        transaction.commit().map_err(map_transaction_error)?;
        maybe_fault(fault_point, FaultPoint::AfterCommit);
        set_private_file(&self.path)?;
        set_private_file(&PathBuf::from(format!("{}-wal", self.path.display())))?;
        set_private_file(&PathBuf::from(format!("{}-shm", self.path.display())))?;
        maybe_fault(fault_point, FaultPoint::BeforeReadback);

        let (stored_digest, stored_bytes): (String, Vec<u8>) = self
            .connection
            .query_row(
                "SELECT sha256, source FROM artifacts WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(map_transaction_error)?;
        if stored_digest != digest || sha256_hex(&stored_bytes) != digest {
            return Err(AppError::new(
                "commit_failed",
                "committed artifact failed readback verification",
            ));
        }
        Ok(ArtifactRef {
            schema_version: ARTIFACT_SCHEMA_VERSION,
            id,
            source_sha256: digest,
            source_bytes: source.len(),
            created_at: now,
            expires_at,
        })
    }

    fn recover(&self, id: &str) -> Result<(ArtifactRef, Vec<u8>), AppError> {
        let record = self.connection.query_row(
            "SELECT source, sha256, source_bytes, created_at, expires_at
             FROM artifacts WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        );
        let (source, digest, source_bytes, created_at, expires_at) = match record {
            Ok(record) => record,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(AppError::new("artifact_unknown", "artifact does not exist"));
            }
            Err(error) => return Err(map_transaction_error(error)),
        };
        let source_bytes = usize::try_from(source_bytes)
            .map_err(|_| AppError::new("artifact_corrupt", "artifact byte count is invalid"))?;
        let created_at = u64::try_from(created_at)
            .map_err(|_| AppError::new("artifact_corrupt", "artifact creation time is invalid"))?;
        let expires_at = u64::try_from(expires_at).map_err(|_| {
            AppError::new("artifact_corrupt", "artifact expiration time is invalid")
        })?;
        let artifact = ArtifactRef {
            schema_version: ARTIFACT_SCHEMA_VERSION,
            id: id.to_owned(),
            source_sha256: digest.clone(),
            source_bytes,
            created_at,
            expires_at,
        };
        if unix_seconds()? >= expires_at {
            return Err(
                AppError::new("artifact_expired", "artifact retention expired")
                    .with_artifact(artifact),
            );
        }
        if source.len() != source_bytes || sha256_hex(&source) != digest {
            return Err(
                AppError::new("artifact_corrupt", "artifact digest mismatch")
                    .with_artifact(artifact),
            );
        }
        Ok((artifact, source))
    }
}

fn map_store_open_error(error: rusqlite::Error) -> AppError {
    match sqlite_code(&error) {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            AppError::new("store_busy", "artifact store is busy")
        }
        Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly) => {
            AppError::new("permission_denied", "artifact store is not writable")
        }
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
            AppError::new("artifact_corrupt", "artifact database is corrupt")
        }
        _ => AppError::new("commit_failed", "artifact store initialization failed"),
    }
}

fn map_transaction_error(error: rusqlite::Error) -> AppError {
    match sqlite_code(&error) {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            AppError::new("store_busy", "artifact transaction timed out")
        }
        Some(ErrorCode::DiskFull) => AppError::new("store_full", "SQLite reported a full store"),
        Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly) => {
            AppError::new("permission_denied", "artifact transaction is not writable")
        }
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
            AppError::new("artifact_corrupt", "artifact database is corrupt")
        }
        _ => AppError::new("commit_failed", "artifact transaction failed"),
    }
}

fn sqlite_code(error: &rusqlite::Error) -> Option<ErrorCode> {
    match error {
        rusqlite::Error::SqliteFailure(code, _) => Some(code.code),
        rusqlite::Error::FromSqlConversionFailure(_, SqlType::Integer, _) => None,
        _ => None,
    }
}

fn maybe_fault(configured: Option<FaultPoint>, current: FaultPoint) {
    if configured == Some(current)
        && env::var_os("DISTILL_SPIKE_ENABLE_FAULTS").as_deref() == Some(std::ffi::OsStr::new("1"))
    {
        process::exit(86);
    }
}

fn unix_seconds() -> Result<u64, AppError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| AppError::new("invariant_breach", "system clock precedes Unix epoch"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn set_private_dir(path: &Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| AppError::new("permission_denied", "cannot enforce store directory mode"))
}

#[cfg(not(unix))]
fn set_private_dir(_path: &Path) -> Result<(), AppError> {
    Err(AppError::new(
        "permission_denied",
        "spike requires POSIX permission enforcement",
    ))
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| AppError::new("permission_denied", "cannot enforce store file mode"))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> Result<(), AppError> {
    Err(AppError::new(
        "permission_denied",
        "spike requires POSIX permission enforcement",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded_facts() -> DecodedFacts {
        (
            vec![("fatal".to_owned(), b"FATAL fact".to_vec())],
            vec![("hint".to_owned(), b"helpful hint".to_vec())],
        )
    }

    #[test]
    fn projection_is_bounded_and_preserves_facts() {
        let source =
            b"header\nnoise noise noise noise\nFATAL fact\nmore noise\nhelpful hint\ntail\n";
        let projection = make_projection(source, 80, &decoded_facts()).expect("projection");
        assert!(projection.visible.len() <= 80);
        assert!(
            projection
                .visible
                .windows(b"FATAL fact".len())
                .any(|window| window == b"FATAL fact")
        );
    }

    #[test]
    fn unsatisfiable_budget_is_typed() {
        let error =
            make_projection(b"FATAL fact\n", 2, &decoded_facts()).expect_err("budget must fail");
        assert_eq!(error.code, "budget_unsatisfiable");
    }

    #[test]
    fn store_round_trip_and_corruption_detection() {
        let root = env::temp_dir().join(format!(
            "distill-rust-spike-test-{}",
            unix_seconds().expect("clock")
        ));
        let path = root.join("store.sqlite");
        let artifact = Store::open(&path, 100)
            .expect("store")
            .capture(b"source bytes", 60, 1_024, None)
            .expect("capture");
        let (_, recovered) = Store::open(&path, 100)
            .expect("store")
            .recover(&artifact.id)
            .expect("recover");
        assert_eq!(recovered, b"source bytes");

        let connection = Connection::open(&path).expect("open");
        connection
            .execute(
                "UPDATE artifacts SET source = ?1 WHERE id = ?2",
                params![b"corrupt", artifact.id],
            )
            .expect("corrupt");
        let error = Store::open(&path, 100)
            .expect("store")
            .recover(&artifact.id)
            .expect_err("digest mismatch");
        assert_eq!(error.code, "artifact_corrupt");
        fs::remove_dir_all(root).expect("cleanup");
    }
}
