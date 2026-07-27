#![allow(clippy::expect_used, clippy::panic)]

use distill::{
    Budget, ByteSpan, ByteString, CL100K_PROFILE, CONTRACT_VERSION, CountUnit, Engine,
    EngineConfig, FailureCode, Fidelity, Outcome, POLICY_VERSION, PROJECTION_VERSION, Request,
    Retention, Source,
};
use sha2::{Digest, Sha256};

fn engine() -> (tempfile::TempDir, Engine) {
    let directory = tempfile::tempdir().expect("temporary store");
    let engine = Engine::new(EngineConfig::local(
        directory.path().join("store/artifacts.sqlite"),
    ))
    .expect("engine");
    (directory, engine)
}

fn request(request_id: &str, source: impl Into<Vec<u8>>, budget: Budget, profile: &str) -> Request {
    Request {
        contract_version: CONTRACT_VERSION.to_owned(),
        request_id: request_id.to_owned(),
        source: Source::Inline {
            bytes: ByteString(source.into()),
            media_type: None,
        },
        budget,
        preservation_profile: profile.to_owned(),
        retention: Retention::default(),
    }
}

fn byte_budget(total: u64, reserved: u64) -> Budget {
    Budget {
        unit: CountUnit::Bytes,
        total_visible_limit: total,
        reserved_envelope: reserved,
        token_profile: None,
    }
}

fn token_budget(total: u64) -> Budget {
    Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: total,
        reserved_envelope: 0,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    }
}

#[test]
fn public_projection_matrix_freezes_fidelity_budget_and_receipt_claims() {
    let (_directory, engine) = engine();

    let exact = engine
        .handle(request(
            "exact-fit",
            b"exact-fit".to_vec(),
            byte_budget(12, 3),
            "plain-text/v1",
        ))
        .expect("exact fit");
    assert_eq!(exact.visible.bytes, "exact-fit");
    assert_eq!(exact.receipt.fidelity, Fidelity::Exact);
    assert_eq!(exact.receipt.original_count, 9);
    assert_eq!(exact.receipt.visible_count, 9);

    let overflow = engine
        .handle(request(
            "one-byte-over",
            b"exact-fit".to_vec(),
            byte_budget(11, 3),
            "plain-text/v1",
        ))
        .expect("one-byte overflow");
    assert_eq!(overflow.visible.bytes, "exact-fi");
    assert_eq!(overflow.receipt.fidelity, Fidelity::Extractive);
    assert_partition(
        9,
        &overflow.receipt.retained_spans,
        &overflow.receipt.omitted_spans,
    );

    let extractive_source =
        b"head\nnoise noise noise\nERROR E1: critical\nwarning W1: useful\ntail\n";
    let extractive = engine
        .handle(request(
            "extractive",
            extractive_source.to_vec(),
            byte_budget(48, 0),
            "build-log/v1",
        ))
        .expect("extractive");
    assert_eq!(extractive.receipt.fidelity, Fidelity::Extractive);
    assert_eq!(
        extractive.receipt.preservation.mandatory_fact_ids,
        ["build-log/v1:23-42"]
    );
    assert_partition(
        extractive_source.len() as u64,
        &extractive.receipt.retained_spans,
        &extractive.receipt.omitted_spans,
    );
    assert_receipt_binding(&extractive, extractive_source, CountUnit::Bytes);

    let encoded = engine
        .handle(request(
            "encoded",
            vec![0xff, b'A'],
            byte_budget(5, 0),
            "none/v1",
        ))
        .expect("encoded");
    assert_eq!(encoded.visible.bytes, "\\xffA");
    assert_eq!(encoded.receipt.fidelity, Fidelity::Encoded);

    let metadata_only = engine
        .handle(request(
            "metadata-only",
            vec![0xff; 64],
            byte_budget(64, 0),
            "none/v1",
        ))
        .expect("metadata only");
    assert_eq!(metadata_only.visible.bytes, "[binary source: 64 bytes]");
    assert_eq!(metadata_only.receipt.fidelity, Fidelity::MetadataOnly);

    let empty = engine
        .handle(request(
            "zero-budget",
            Vec::new(),
            byte_budget(0, 0),
            "plain-text/v1",
        ))
        .expect("empty zero budget");
    assert_eq!(empty.visible.bytes, "");
    assert_eq!(empty.receipt.visible_count, 0);
    assert_eq!(empty.receipt.fidelity, Fidelity::Exact);
}

#[test]
fn binary_mandatory_budget_failure_commits_source_without_visible_projection() {
    let (_directory, engine) = engine();
    let source = b"fatal_\xff".to_vec();
    let failure = engine
        .handle(request(
            "binary-mandatory",
            source.clone(),
            byte_budget(9, 0),
            "binary/v1",
        ))
        .expect_err("mandatory representation cannot fit");
    assert_eq!(failure.code, FailureCode::BudgetUnsatisfiable);
    let artifact = failure.artifact.expect("committed artifact");
    let restored = engine.restore(&artifact).expect("restore committed source");
    assert_eq!(restored.bytes.0, source);
}

#[test]
fn repeated_public_runs_have_byte_identical_projection_and_equivalent_claims() {
    let (_directory, engine) = engine();
    let candidate = request(
        "repeat",
        b"head\nnoise noise noise\nERROR E1: critical\nwarning W1: useful\ntail\n".to_vec(),
        byte_budget(48, 0),
        "build-log/v1",
    );
    let first = engine.handle(candidate.clone()).expect("first");
    let second = engine.handle(candidate).expect("second");

    assert_eq!(first.visible, second.visible);
    assert_eq!(first.receipt.request_id, second.receipt.request_id);
    assert_eq!(first.receipt.source_sha256, second.receipt.source_sha256);
    assert_eq!(
        first.receipt.projection_version,
        second.receipt.projection_version
    );
    assert_eq!(first.receipt.policy_version, second.receipt.policy_version);
    assert_eq!(first.receipt.token_profile, second.receipt.token_profile);
    assert_eq!(first.receipt.original_count, second.receipt.original_count);
    assert_eq!(first.receipt.visible_count, second.receipt.visible_count);
    assert_eq!(first.receipt.count_unit, second.receipt.count_unit);
    assert_eq!(first.receipt.fidelity, second.receipt.fidelity);
    assert_eq!(first.receipt.retained_spans, second.receipt.retained_spans);
    assert_eq!(first.receipt.omitted_spans, second.receipt.omitted_spans);
    assert_eq!(first.receipt.preservation, second.receipt.preservation);
    assert_eq!(first.receipt.acquisition, second.receipt.acquisition);
}

#[test]
fn immutable_cl100k_vectors_are_verified_through_engine_handle() {
    const VECTORS: [(&str, u64); 20] = [
        ("", 0),
        ("a", 1),
        ("A", 1),
        ("0", 1),
        (" ", 1),
        ("\n", 1),
        ("hello", 1),
        (" world", 1),
        ("hello world", 2),
        ("Hello, world!", 4),
        ("true", 1),
        ("false", 1),
        ("null", 1),
        ("{}", 1),
        ("[]", 1),
        ("hello\nworld", 3),
        ("rust", 1),
        ("distill", 2),
        ("antidisestablishmentarianism", 6),
        ("お誕生日おめでとう", 9),
    ];

    let (_directory, engine) = engine();
    for (index, (input, expected)) in VECTORS.into_iter().enumerate() {
        let outcome = engine
            .handle(request(
                &format!("token-vector-{index}"),
                input.as_bytes().to_vec(),
                token_budget(expected),
                "plain-text/v1",
            ))
            .unwrap_or_else(|failure| panic!("vector {index} failed: {failure}"));
        assert_eq!(outcome.receipt.fidelity, Fidelity::Exact, "vector {index}");
        assert_eq!(
            outcome.receipt.original_count, expected,
            "vector {index}: {input:?}"
        );
        assert_eq!(
            outcome.receipt.visible_count, expected,
            "vector {index}: {input:?}"
        );
        assert_eq!(outcome.receipt.count_unit, CountUnit::Tokens);
        assert_eq!(
            outcome.receipt.token_profile.as_deref(),
            Some(CL100K_PROFILE)
        );
    }
}

fn assert_receipt_binding(outcome: &Outcome, source: &[u8], unit: CountUnit) {
    assert_eq!(
        outcome.receipt.source_sha256,
        format!("{:x}", Sha256::digest(source))
    );
    assert_eq!(outcome.receipt.artifact, outcome.artifact);
    assert_eq!(
        outcome.receipt.source_sha256,
        outcome.artifact.source_sha256
    );
    assert_eq!(outcome.receipt.projection_version, PROJECTION_VERSION);
    assert_eq!(outcome.receipt.policy_version, POLICY_VERSION);
    assert_eq!(outcome.receipt.count_unit, unit);
}

fn assert_partition(source_bytes: u64, retained: &[ByteSpan], omitted: &[ByteSpan]) {
    assert_sorted_nonoverlapping(retained);
    assert_sorted_nonoverlapping(omitted);

    let mut spans = retained.iter().chain(omitted).copied().collect::<Vec<_>>();
    spans.sort_by_key(|span| span.start);
    let mut cursor = 0;
    for span in spans {
        assert_eq!(span.start, cursor);
        assert!(span.end > span.start);
        cursor = span.end;
    }
    assert_eq!(cursor, source_bytes);
}

fn assert_sorted_nonoverlapping(spans: &[ByteSpan]) {
    assert!(
        spans
            .windows(2)
            .all(|pair| pair[0].start <= pair[1].start && pair[0].end <= pair[1].start)
    );
}
