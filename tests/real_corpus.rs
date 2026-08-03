//! EP-002 evidence on the executed path: the real tool-output corpus projected
//! through `Engine::handle` at the budget the Codex hook runs, measured against
//! the frozen `evaluation/baseline/projection-baseline-v1.json` behavior.

#![allow(clippy::expect_used, clippy::panic)]

use distill::{
    Budget, ByteSpan, ByteString, CL100K_PROFILE, CONTRACT_VERSION, CountUnit, Engine,
    EngineConfig, Fidelity, Request, Retention, Source,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

const REAL_CORPUS_SCHEMA_VERSION: &str = "distill.real-corpus/v1";
const EXECUTED_PROFILE: &str = "plain-text/v1";

/// The executed Codex hook default: 2250 total visible tokens with a 450-token
/// reserved envelope, so 1800 tokens of payload.
const HOOK_TOTAL_VISIBLE_LIMIT: u64 = 2_250;
const HOOK_RESERVED_ENVELOPE: u64 = 450;
const HOOK_PAYLOAD_LIMIT: u64 = HOOK_TOTAL_VISIBLE_LIMIT - HOOK_RESERVED_ENVELOPE;

/// EP-002 requires a median budget utilization of at least 85% on over-budget
/// real fixtures. The frozen baseline median is 0.7778%.
const UTILIZATION_FLOOR_BASIS_POINTS: u64 = 8_500;
const BASELINE_MEDIAN_UTILIZATION_BASIS_POINTS: u64 = 77;

#[derive(Debug, Deserialize)]
struct RealFixture {
    schema_version: String,
    id: String,
    #[allow(dead_code)]
    shape: String,
    byte_length: usize,
    sha256: String,
    path: String,
}

fn engine() -> (tempfile::TempDir, Engine) {
    let directory = tempfile::tempdir().expect("temporary store");
    let engine = Engine::new(EngineConfig::local(
        directory.path().join("store/artifacts.sqlite"),
    ))
    .expect("engine");
    (directory, engine)
}

fn hook_budget() -> Budget {
    Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: HOOK_TOTAL_VISIBLE_LIMIT,
        reserved_envelope: HOOK_RESERVED_ENVELOPE,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    }
}

fn request(id: &str, bytes: Vec<u8>) -> Request {
    Request {
        contract_version: CONTRACT_VERSION.to_owned(),
        request_id: id.to_owned(),
        source: Source::Inline {
            bytes: ByteString(bytes),
            media_type: None,
        },
        budget: hook_budget(),
        preservation_profile: EXECUTED_PROFILE.to_owned(),
        retention: Retention::default(),
    }
}

fn real_corpus() -> Vec<(RealFixture, Vec<u8>)> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("evaluation/corpus/real");
    let manifest =
        fs::read_to_string(directory.join("manifest.jsonl")).expect("real corpus manifest");
    let fixtures = manifest
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fixture: RealFixture = serde_json::from_str(line).expect("real corpus record");
            assert_eq!(fixture.schema_version, REAL_CORPUS_SCHEMA_VERSION);
            let bytes = fs::read(directory.join(&fixture.path)).expect("fixture bytes");
            assert_eq!(bytes.len(), fixture.byte_length, "{}", fixture.id);
            assert_eq!(
                format!("{:x}", Sha256::digest(&bytes)),
                fixture.sha256,
                "{} digest",
                fixture.id
            );
            (fixture, bytes)
        })
        .collect::<Vec<_>>();
    assert!(fixtures.len() >= 40, "{} real fixtures", fixtures.len());
    fixtures
}

/// EP-002 definition of done, measured through the single policy seam: the
/// executed profile spends its payload budget on over-budget real output, holds
/// the receipt span ceiling, and keeps the partition exact.
#[test]
fn executed_path_spends_its_payload_budget_on_the_real_corpus() {
    let (_directory, engine) = engine();
    let mut utilization = Vec::new();
    let mut over_budget = 0;

    for (fixture, bytes) in real_corpus() {
        let outcome = engine
            .handle(request(&fixture.id, bytes.clone()))
            .unwrap_or_else(|failure| panic!("{} failed: {failure}", fixture.id));
        let receipt = &outcome.receipt;

        assert!(
            receipt.visible_count.saturating_add(HOOK_RESERVED_ENVELOPE)
                <= HOOK_TOTAL_VISIBLE_LIMIT,
            "{} exceeded the total visible limit",
            fixture.id
        );
        assert!(receipt.retained_spans.len() <= 257, "{}", fixture.id);
        assert!(receipt.omitted_spans.len() <= 257, "{}", fixture.id);
        assert_partition(
            bytes.len() as u64,
            &receipt.retained_spans,
            &receipt.omitted_spans,
            &fixture.id,
        );

        if receipt.original_count <= HOOK_PAYLOAD_LIMIT {
            assert_eq!(receipt.fidelity, Fidelity::Exact, "{}", fixture.id);
            assert!(receipt.omitted_spans.is_empty(), "{}", fixture.id);
            continue;
        }

        over_budget += 1;
        assert_eq!(receipt.fidelity, Fidelity::Extractive, "{}", fixture.id);
        let measured = basis_points(receipt.visible_count, HOOK_PAYLOAD_LIMIT);
        utilization.push((measured, fixture.id.clone()));
    }

    assert!(
        over_budget >= 14,
        "{over_budget} over-budget fixtures, fewer than the 14 the baseline recorded"
    );
    utilization.sort_unstable();
    let median = utilization[(utilization.len() - 1) / 2].0;
    eprintln!(
        "executed path over {over_budget} over-budget fixtures: min={} median={median} max={}",
        utilization[0].0,
        utilization.last().expect("utilization").0
    );
    assert!(
        median > BASELINE_MEDIAN_UTILIZATION_BASIS_POINTS,
        "median utilization did not move off the frozen baseline"
    );
    assert!(
        median >= UTILIZATION_FLOOR_BASIS_POINTS,
        "median budget utilization was {}.{:02}%, below the {}% floor; lowest fixture {}",
        median / 100,
        median % 100,
        UTILIZATION_FLOOR_BASIS_POINTS / 100,
        utilization[0].1
    );
}

/// The two fixtures the PRD problem statement measures, whose frozen baseline
/// utilization is 0.39% and 2.44%.
#[test]
fn the_measured_fixtures_reproduce_the_recorded_gain() {
    let (_directory, engine) = engine();
    let corpus = real_corpus();
    for (id, baseline_visible_count) in [("source-artifact-rs", 7), ("log-git-log-stat", 44)] {
        let (_, bytes) = corpus
            .iter()
            .find(|(fixture, _)| fixture.id == id)
            .unwrap_or_else(|| panic!("fixture {id}"));
        let outcome = engine
            .handle(request(id, bytes.clone()))
            .unwrap_or_else(|failure| panic!("{id} failed: {failure}"));
        let measured = basis_points(outcome.receipt.visible_count, HOOK_PAYLOAD_LIMIT);
        assert!(
            outcome.receipt.visible_count > baseline_visible_count,
            "{id} did not improve on its {baseline_visible_count}-token baseline"
        );
        assert!(
            measured >= UTILIZATION_FLOOR_BASIS_POINTS,
            "{id} utilization was {}.{:02}%",
            measured / 100,
            measured % 100
        );
    }
}

/// A projection that ignores its budget is not the only regression worth
/// catching: identical input must keep producing an identical payload.
#[test]
fn repeated_real_corpus_projections_are_byte_identical() {
    let (_directory, engine) = engine();
    for (fixture, bytes) in real_corpus() {
        let first = engine
            .handle(request(&fixture.id, bytes.clone()))
            .expect("first projection");
        let second = engine
            .handle(request(&fixture.id, bytes))
            .expect("repeated projection");
        assert_eq!(first.visible, second.visible, "{}", fixture.id);
        assert_eq!(
            first.receipt.retained_spans, second.receipt.retained_spans,
            "{}",
            fixture.id
        );
        assert_eq!(
            first.receipt.visible_count, second.receipt.visible_count,
            "{}",
            fixture.id
        );
    }
}

fn basis_points(part: u64, whole: u64) -> u64 {
    (part * 10_000).checked_div(whole).unwrap_or(0)
}

fn assert_partition(source_bytes: u64, retained: &[ByteSpan], omitted: &[ByteSpan], id: &str) {
    let mut spans = retained.iter().chain(omitted).copied().collect::<Vec<_>>();
    spans.sort_by_key(|span| span.start);
    let mut cursor = 0;
    for span in spans {
        assert_eq!(span.start, cursor, "{id} partition gap or overlap");
        assert!(span.end > span.start, "{id} empty span");
        cursor = span.end;
    }
    assert_eq!(cursor, source_bytes, "{id} partition does not cover source");
}
