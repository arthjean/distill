use distill::{AcquisitionReceipt, ArtifactRef, Outcome};
use serde_json::{Map, Value, json};

pub(crate) fn projection_envelope(
    schema_version: &'static str,
    outcome: &Outcome,
    acquisition: Option<&AcquisitionReceipt>,
) -> Value {
    let mut envelope = Map::new();
    envelope.insert("schema_version".to_owned(), json!(schema_version));
    envelope.insert("source_is_untrusted".to_owned(), json!(true));
    envelope.insert("projection".to_owned(), json!(outcome.visible.bytes));
    envelope.insert("artifact".to_owned(), json!(outcome.artifact));
    envelope.insert("fidelity".to_owned(), json!(outcome.receipt.fidelity));
    envelope.insert(
        "accounting".to_owned(),
        json!({
            "original": outcome.receipt.original_count,
            "visible": outcome.receipt.visible_count,
            "unit": outcome.receipt.count_unit,
            "token_profile": outcome.receipt.token_profile,
        }),
    );
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
    envelope.insert(
        "recovery".to_owned(),
        json!(format!("distill artifact get {}", outcome.artifact.id)),
    );
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
    )
}

fn serialize_error(
    schema_version: &'static str,
    code: &str,
    message: &str,
    artifact: Option<&ArtifactRef>,
    safety_field: &'static str,
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
        json!(artifact.map(|value| format!("distill artifact get {}", value.id))),
    );
    serde_json::to_string(&Value::Object(envelope))
        .unwrap_or_else(|_| "{\"error\":{\"code\":\"invariant_breach\"}}".to_owned())
}

#[cfg(test)]
mod tests;
