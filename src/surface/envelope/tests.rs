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
                profile: distill::AUTO_PROFILE.to_owned(),
                applied_profile: "terminal-log/v1".to_owned(),
                mandatory_fact_ids: Vec::new(),
                aggregates: Vec::new(),
                focus_applied: false,
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

fn omitting(fidelity: Fidelity) -> Outcome {
    let mut outcome = outcome();
    outcome.receipt.fidelity = fidelity;
    outcome.receipt.original_count = 120;
    outcome.receipt.visible_count = 7;
    outcome
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
    });
    assert_eq!(
        serde_json::to_string(&projection_envelope(
            "distill.codex-projection/v1",
            &outcome,
            None,
            Recovery::CliCommands,
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
    });
    assert_eq!(
        serde_json::to_string(&projection_envelope(
            "distill.mcp/v1",
            &outcome,
            Some(&outcome.receipt.acquisition),
            Recovery::McpTools,
        ))
        .expect("shared MCP envelope"),
        serde_json::to_string(&expected_mcp).expect("expected MCP envelope"),
    );
}

/// US-009: an exact projection omits nothing, so it advertises no recovery and
/// reports no omission.
#[test]
fn an_exact_projection_emits_no_recovery_instruction() {
    for recovery in [Recovery::McpTools, Recovery::CliCommands] {
        let envelope = projection_envelope("distill.mcp/v1", &outcome(), None, recovery);
        assert!(envelope.get("recovery").is_none());
        assert!(envelope["accounting"].get("omitted").is_none());
    }
}

/// US-009: an omitting projection reports the omission in the request's count
/// unit and names the bounded operation available on its own surface.
#[test]
fn an_omitting_projection_names_a_bounded_surface_operation() {
    let outcome = omitting(Fidelity::Extractive);
    let id = &outcome.artifact.id;

    let mcp = projection_envelope(
        "distill.mcp/v1",
        &outcome,
        Some(&outcome.receipt.acquisition),
        Recovery::McpTools,
    );
    assert_eq!(mcp["accounting"]["omitted"], 113);
    assert_eq!(mcp["accounting"]["unit"], "bytes");
    let mcp_recovery = mcp["recovery"].as_str().expect("MCP recovery");
    assert!(mcp_recovery.contains("distill_artifact_slice"));
    assert!(mcp_recovery.contains("distill_artifact_search"));
    assert!(mcp_recovery.contains(id));
    assert!(!mcp_recovery.contains("artifact get"));

    let codex = projection_envelope(
        "distill.codex-projection/v1",
        &outcome,
        None,
        Recovery::CliCommands,
    );
    assert_eq!(codex["accounting"]["omitted"], 113);
    let codex_recovery = codex["recovery"].as_str().expect("Codex recovery");
    assert!(codex_recovery.contains("distill artifact slice"));
    assert!(codex_recovery.contains("--budget"));
    assert!(codex_recovery.contains(id));
    assert!(!codex_recovery.contains("artifact get"));
}

/// US-009: a committed artifact whose bytes bounded retrieval cannot select
/// still reports its omission, without naming an operation that would fail.
#[test]
fn an_unretrievable_projection_states_omission_without_an_operation() {
    for fidelity in [Fidelity::Encoded, Fidelity::MetadataOnly] {
        let outcome = omitting(fidelity);
        let envelope = projection_envelope(
            "distill.mcp/v1",
            &outcome,
            Some(&outcome.receipt.acquisition),
            Recovery::McpTools,
        );
        assert!(envelope["artifact"].is_object());
        assert_eq!(envelope["accounting"]["omitted"], 113);
        assert!(envelope.get("recovery").is_none());
    }
}

#[test]
fn shared_error_envelopes_name_bounded_recovery_per_surface() {
    let outcome = outcome();
    let expected = json!({
        "schema_version": "distill.mcp/v1",
        "error": {
            "code": "store_full",
            "message": "bounded",
            "artifact": outcome.artifact,
        },
        "raw_content_included": false,
        "recovery": Recovery::McpTools.instruction(&outcome.artifact),
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

    let codex = codex_error_envelope(
        "distill.codex-projection/v1",
        "budget_unsatisfiable",
        "bounded",
        Some(&outcome.artifact),
    );
    assert!(codex.contains("distill artifact slice"));
    assert!(!codex.contains("artifact get"));

    // Without a committed artifact there is nothing to retrieve.
    let uncommitted: Value = serde_json::from_str(&mcp_error_envelope(
        "distill.mcp/v1",
        "invalid_request",
        "x",
        None,
    ))
    .expect("uncommitted envelope");
    assert_eq!(uncommitted["recovery"], Value::Null);
}
