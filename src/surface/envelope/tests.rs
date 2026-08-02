use super::*;
use distill::{
    AcquisitionReceipt, ArtifactRef, CountUnit, Fidelity, Outcome, PreservationResult, Receipt,
    SourceVariant, VisiblePayload,
};

fn outcome() -> Outcome {
    let artifact = ArtifactRef {
        schema_version: "distill.artifact/v1".to_owned(),
        id: "0123456789abcdef0123456789abcdef".to_owned(),
        source_sha256: "a".repeat(64),
        source_bytes: 7,
        created_at: 10,
        expires_at: 20,
    };
    Outcome {
        visible: VisiblePayload {
            bytes: "visible".to_owned(),
            media_type: "text/plain".to_owned(),
        },
        artifact: artifact.clone(),
        receipt: Receipt {
            schema_version: "distill.receipt/v1".to_owned(),
            request_id: "request".to_owned(),
            source_sha256: artifact.source_sha256.clone(),
            artifact,
            projection_version: "projection/v1".to_owned(),
            policy_version: "policy/v1".to_owned(),
            token_profile: None,
            original_count: 7,
            visible_count: 7,
            count_unit: CountUnit::Bytes,
            fidelity: Fidelity::Exact,
            retained_spans: Vec::new(),
            omitted_spans: Vec::new(),
            preservation: PreservationResult {
                profile: "plain-text/v1".to_owned(),
                mandatory_fact_ids: Vec::new(),
            },
            acquisition: AcquisitionReceipt {
                variant: SourceVariant::File,
                complete: true,
                partial: false,
                truncated: false,
                root_id: Some("workspace".to_owned()),
                relative_path: Some("<8 path bytes>".to_owned()),
                process: None,
            },
        },
    }
}

#[test]
fn shared_projection_envelopes_preserve_existing_wire_encoding() {
    let outcome = outcome();
    let expected_codex = json!({
        "schema_version": "distill.codex-projection/v1",
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
        "recovery": format!("distill artifact get {}", outcome.artifact.id),
    });
    assert_eq!(
        serde_json::to_string(&projection_envelope(
            "distill.codex-projection/v1",
            &outcome,
            None,
        ))
        .expect("shared Codex envelope"),
        serde_json::to_string(&expected_codex).expect("expected Codex envelope"),
    );

    let process = Value::Null;
    let expected_mcp = json!({
        "schema_version": "distill.mcp/v1",
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
    });
    assert_eq!(
        serde_json::to_string(&projection_envelope(
            "distill.mcp/v1",
            &outcome,
            Some(&outcome.receipt.acquisition),
        ))
        .expect("shared MCP envelope"),
        serde_json::to_string(&expected_mcp).expect("expected MCP envelope"),
    );
}

#[test]
fn shared_error_envelopes_preserve_existing_wire_encoding() {
    let outcome = outcome();
    let expected = json!({
        "schema_version": "distill.mcp/v1",
        "error": {
            "code": "store_full",
            "message": "bounded",
            "artifact": outcome.artifact,
        },
        "raw_content_included": false,
        "recovery": format!("distill artifact get {}", outcome.artifact.id),
    });
    assert_eq!(
        mcp_error_envelope(
            "distill.mcp/v1",
            "store_full",
            "bounded",
            Some(&outcome.artifact),
        ),
        serde_json::to_string(&expected).expect("expected envelope"),
    );
}
