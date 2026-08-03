use distill::{AcquisitionReceipt, ArtifactRef, Fidelity, Outcome, Receipt};
use serde_json::{Map, Value, json};

/// The bounded retrieval operation a surface can offer for what it omitted.
/// Every surface that can return an omitting projection must name one, and the
/// envelope names none when the omitted content cannot be retrieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
    McpTools,
    CliCommands,
}

impl Recovery {
    fn instruction(self, artifact: &ArtifactRef) -> String {
        let id = &artifact.id;
        match self {
            Self::McpTools => format!(
                "call distill_artifact_slice or distill_artifact_search with artifact_id {id} and a budget"
            ),
            Self::CliCommands => format!(
                "distill artifact slice {id} --start-line N --lines N --budget N, or distill artifact search {id} --pattern TEXT --budget N"
            ),
        }
    }

    /// Whether bounded retrieval can serve this outcome. Selection is line and
    /// literal-text based, so an encoded or metadata-only projection of a
    /// non-UTF-8 source states its omission without naming an operation that
    /// would fail.
    fn serves(self, receipt: &Receipt) -> bool {
        receipt.fidelity == Fidelity::Extractive
    }
}

pub(crate) fn projection_envelope(
    schema_version: &'static str,
    outcome: &Outcome,
    acquisition: Option<&AcquisitionReceipt>,
    recovery: Recovery,
) -> Value {
    let mut envelope = Map::new();
    envelope.insert("schema_version".to_owned(), json!(schema_version));
    envelope.insert("source_is_untrusted".to_owned(), json!(true));
    envelope.insert("projection".to_owned(), json!(outcome.visible.bytes));
    envelope.insert("artifact".to_owned(), json!(outcome.artifact));
    envelope.insert("fidelity".to_owned(), json!(outcome.receipt.fidelity));
    let mut accounting = Map::from_iter([
        ("original".to_owned(), json!(outcome.receipt.original_count)),
        ("visible".to_owned(), json!(outcome.receipt.visible_count)),
        ("unit".to_owned(), json!(outcome.receipt.count_unit)),
        (
            "token_profile".to_owned(),
            json!(outcome.receipt.token_profile),
        ),
    ]);
    if outcome.receipt.fidelity != Fidelity::Exact {
        accounting.insert(
            "omitted".to_owned(),
            json!(
                outcome
                    .receipt
                    .original_count
                    .saturating_sub(outcome.receipt.visible_count)
            ),
        );
    }
    envelope.insert("accounting".to_owned(), Value::Object(accounting));
    envelope.insert(
        "receipt".to_owned(),
        json!({
            "schema_version": outcome.receipt.schema_version,
            "source_sha256": outcome.receipt.source_sha256,
            "projection_version": outcome.receipt.projection_version,
            "policy_version": outcome.receipt.policy_version,
        }),
    );
    if let Some(acquisition) = acquisition {
        let process = acquisition.process.as_ref().map(|process| {
            json!({
                "exit_code": process.exit_code,
                "signal": process.signal,
                "timed_out": process.timed_out,
                "working_directory": process.working_directory,
            })
        });
        envelope.insert(
            "source".to_owned(),
            json!({
                "variant": acquisition.variant,
                "complete": acquisition.complete,
                "partial": acquisition.partial,
                "truncated": acquisition.truncated,
                "root_id": acquisition.root_id,
                "relative_path": acquisition.relative_path,
                "process": process,
            }),
        );
    }
    if recovery.serves(&outcome.receipt) {
        envelope.insert(
            "recovery".to_owned(),
            json!(recovery.instruction(&outcome.artifact)),
        );
    }
    Value::Object(envelope)
}

pub(crate) fn mcp_error_envelope(
    schema_version: &'static str,
    code: &str,
    message: &str,
    artifact: Option<&ArtifactRef>,
) -> String {
    serialize_error(
        schema_version,
        code,
        message,
        artifact,
        "raw_content_included",
        Recovery::McpTools,
    )
}

pub(crate) fn codex_error_envelope(
    schema_version: &'static str,
    code: &str,
    message: &str,
    artifact: Option<&ArtifactRef>,
) -> String {
    serialize_error(
        schema_version,
        code,
        message,
        artifact,
        "raw_output_forwarded_as_projected",
        Recovery::CliCommands,
    )
}

fn serialize_error(
    schema_version: &'static str,
    code: &str,
    message: &str,
    artifact: Option<&ArtifactRef>,
    safety_field: &'static str,
    recovery: Recovery,
) -> String {
    let mut envelope = Map::new();
    envelope.insert("schema_version".to_owned(), json!(schema_version));
    envelope.insert(
        "error".to_owned(),
        json!({
            "code": code,
            "message": message.chars().take(512).collect::<String>(),
            "artifact": artifact,
        }),
    );
    envelope.insert(safety_field.to_owned(), json!(false));
    envelope.insert(
        "recovery".to_owned(),
        json!(artifact.map(|value| recovery.instruction(value))),
    );
    serde_json::to_string(&Value::Object(envelope))
        .unwrap_or_else(|_| "{\"error\":{\"code\":\"invariant_breach\"}}".to_owned())
}

#[cfg(test)]
mod tests;
