#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use distill_core::{
    Budget, ByteString, CL100K_PROFILE, CONTRACT_VERSION, CountUnit, Engine, EngineConfig,
    Fidelity, Request, Retention, ScalarValue, Source,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Debug, Deserialize)]
struct Fixture {
    id: String,
    category: String,
    source: Value,
    source_sha256: String,
    budget_profile: String,
    annotations: Annotations,
}

#[derive(Debug, Deserialize)]
struct Annotations {
    p0: Vec<Fact>,
    p1: Vec<Fact>,
}

#[derive(Debug, Deserialize)]
struct Fact {
    needle_base64: String,
}

#[derive(Debug, Deserialize)]
struct BudgetFile {
    profiles: Vec<BudgetProfile>,
}

#[derive(Debug, Deserialize)]
struct BudgetProfile {
    id: String,
    unit: String,
    total_visible_limit: u64,
    reserved_envelope: u64,
    token_profile: Option<String>,
}

#[test]
fn full_annotated_corpus_preserves_p0_and_measures_p1_under_budget() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest =
        fs::read_to_string(root.join("evaluation/corpus/manifest.jsonl")).expect("corpus manifest");
    let profiles: BudgetFile = serde_json::from_slice(
        &fs::read(root.join("evaluation/corpus/budget-profiles.json")).expect("budget profiles"),
    )
    .expect("decode budget profiles");
    let budgets = profiles
        .profiles
        .into_iter()
        .map(|profile| (profile.id.clone(), profile))
        .collect::<BTreeMap<_, _>>();
    let directory = tempfile::tempdir().expect("temp directory");
    let engine = Engine::new(EngineConfig::local(
        directory.path().join("store/store.sqlite"),
    ))
    .expect("engine");

    let fixtures = manifest
        .lines()
        .map(|line| serde_json::from_str::<Fixture>(line).expect("fixture"))
        .collect::<Vec<_>>();
    assert!(fixtures.len() >= 100);
    let mut p1_total = 0_u64;
    let mut p1_preserved = 0_u64;

    for fixture in fixtures {
        let bytes = materialize_source(&fixture.source);
        assert_eq!(sha256_hex(&bytes), fixture.source_sha256);
        let profile = preservation_profile(&fixture.category);
        let budget_profile = budgets
            .get(&fixture.budget_profile)
            .expect("fixture budget");
        if budget_profile.unit == "tokens" {
            assert_eq!(
                budget_profile.token_profile.as_deref(),
                Some(CL100K_PROFILE)
            );
        }
        let budget = Budget {
            unit: match budget_profile.unit.as_str() {
                "bytes" => CountUnit::Bytes,
                "tokens" => CountUnit::Tokens,
                unit => panic!("unknown budget unit: {unit}"),
            },
            total_visible_limit: budget_profile.total_visible_limit,
            reserved_envelope: budget_profile.reserved_envelope,
            token_profile: budget_profile.token_profile.clone(),
        };
        let request = Request {
            contract_version: CONTRACT_VERSION.to_owned(),
            request_id: fixture.id.clone(),
            source: Source::Inline {
                bytes: ByteString(bytes),
                media_type: None,
            },
            budget: budget.clone(),
            preservation_profile: profile.to_owned(),
            retention: Retention::default(),
            metadata: BTreeMap::from([(
                "content_class".to_owned(),
                ScalarValue::String(fixture.category.clone()),
            )]),
        };
        let outcome = engine.handle(request).unwrap_or_else(|failure| {
            panic!(
                "{} failed with {}: {}",
                fixture.id,
                failure.code.as_str(),
                failure.safe_message
            )
        });
        assert_eq!(outcome.receipt.source_sha256, fixture.source_sha256);
        assert!(
            outcome
                .receipt
                .visible_count
                .saturating_add(budget.reserved_envelope)
                <= budget.total_visible_limit,
            "{} exceeded budget",
            fixture.id
        );
        for fact in fixture.annotations.p0 {
            let needle = BASE64.decode(fact.needle_base64).expect("P0 base64");
            assert!(
                outcome
                    .visible
                    .bytes
                    .as_bytes()
                    .windows(needle.len())
                    .any(|window| window == needle),
                "{} omitted a P0 fact under {}",
                fixture.id,
                profile
            );
        }
        for fact in fixture.annotations.p1 {
            p1_total += 1;
            let needle = BASE64.decode(fact.needle_base64).expect("P1 base64");
            if outcome
                .visible
                .bytes
                .as_bytes()
                .windows(needle.len())
                .any(|window| window == needle)
            {
                p1_preserved += 1;
            }
        }
        if outcome.receipt.fidelity == Fidelity::Extractive {
            assert_spans_cover_source(
                outcome.artifact.source_bytes,
                &outcome.receipt.retained_spans,
                &outcome.receipt.omitted_spans,
            );
        }
    }
    assert!(
        p1_preserved * 100 >= p1_total * 95,
        "P1 recall was {p1_preserved}/{p1_total}"
    );
}

fn preservation_profile(category: &str) -> &'static str {
    match category {
        "build-output" | "logs" => "build-log/v1",
        "test-output" => "test-log/v1",
        "diff" => "diff/v1",
        "diagnostics" => "diagnostic/v1",
        "stack-trace" => "stack-trace/v1",
        "source-code" => "source-code/v1",
        "json" => "json/v1",
        "unicode" => "unicode/v1",
        "malformed-bytes" => "binary/v1",
        "prompt-injection" => "untrusted-text/v1",
        "empty" | "boundary" => "plain-text/v1",
        category => panic!("unknown corpus category: {category}"),
    }
}

fn materialize_source(source: &Value) -> Vec<u8> {
    match source.get("kind").and_then(Value::as_str) {
        Some("inline" | "file") => materialize_payload(&source["payload"]),
        Some("process") => source["events"]
            .as_array()
            .expect("process events")
            .iter()
            .flat_map(|event| materialize_payload(&event["payload"]))
            .collect(),
        kind => panic!("unknown source kind: {kind:?}"),
    }
}

fn materialize_payload(payload: &Value) -> Vec<u8> {
    match payload.get("kind").and_then(Value::as_str) {
        Some("utf8") => payload["value"]
            .as_str()
            .expect("UTF-8 payload")
            .as_bytes()
            .to_vec(),
        Some("base64") => BASE64
            .decode(payload["value"].as_str().expect("base64 payload"))
            .expect("canonical base64"),
        Some("padded") => {
            let prefix = BASE64
                .decode(payload["prefix_base64"].as_str().expect("padded prefix"))
                .expect("prefix base64");
            let byte_length = payload["byte_length"].as_u64().expect("byte length") as usize;
            let fill = payload["fill_byte"].as_u64().expect("fill byte") as u8;
            let mut bytes = prefix;
            bytes.resize(byte_length, fill);
            bytes
        }
        kind => panic!("unknown payload kind: {kind:?}"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn assert_spans_cover_source(
    source_bytes: u64,
    retained: &[distill_core::ByteSpan],
    omitted: &[distill_core::ByteSpan],
) {
    let mut spans = retained
        .iter()
        .chain(omitted.iter())
        .copied()
        .collect::<Vec<_>>();
    spans.sort_by_key(|span| span.start);
    let mut cursor = 0;
    for span in spans {
        assert_eq!(span.start, cursor);
        assert!(span.end >= span.start);
        cursor = span.end;
    }
    assert_eq!(cursor, source_bytes);
}
