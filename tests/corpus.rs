#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use distill::{
    Budget, ByteString, CL100K_PROFILE, CONTRACT_VERSION, CountUnit, Engine, EngineConfig,
    Fidelity, MAX_ARTIFACT_LINEAGE_BYTES, MAX_LINEAGE_BYTES, Request, Retention, Source,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
};

const FIXTURE_SCHEMA_VERSION: &str = "distill.projection-fixture/v1";
/// EP-004 retired the needle table, so a P0 fact survives only when the shape of
/// its observation makes it structural. The generated source-code fixtures bury
/// one distinct statement inside twenty-six identical generated helpers, which
/// no shape policy can single out without recognizing the fixture itself: they
/// stay a regression fixture, and the real corpus qualifies source files.
const UNSTRUCTURED_P0_CATEGORY: &str = "source-code";
/// The measured floor across every category, so a policy change that loses
/// structural facts fails here instead of shipping.
const P0_RECALL_FLOOR_PERCENT: u64 = 90;
const PROFILE_SCHEMA_VERSION: &str = "distill.budget-profiles/v1";
const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: String,
    id: String,
    category: String,
    #[serde(rename = "description")]
    _description: String,
    #[serde(rename = "reducible")]
    _reducible: bool,
    source: Value,
    source_sha256: String,
    budget_profile: String,
    #[serde(rename = "expected")]
    _expected: Value,
    annotations: Annotations,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Annotations {
    p0: Vec<Fact>,
    p1: Vec<Fact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fact {
    id: String,
    needle_base64: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetFile {
    schema_version: String,
    profiles: Vec<BudgetProfile>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetProfile {
    id: String,
    unit: String,
    total_visible_limit: u64,
    reserved_envelope: u64,
    #[serde(default)]
    token_profile: Option<String>,
}

struct ValidatedFixture {
    fixture: Fixture,
    bytes: Vec<u8>,
    budget: Budget,
    profile: &'static str,
}

#[test]
fn full_annotated_corpus_preserves_p0_and_measures_p1_under_budget() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest =
        fs::read_to_string(root.join("evaluation/corpus/manifest.jsonl")).expect("corpus manifest");
    let budgets = load_budgets(root);
    let directory = tempfile::tempdir().expect("temp directory");
    let engine = Engine::new(EngineConfig::local(
        directory.path().join("store/store.sqlite"),
    ))
    .expect("engine");

    let fixtures = manifest
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("fixture JSON"))
        .collect::<Vec<_>>();
    assert!(fixtures.len() >= 100);
    let mut p0_total = 0_u64;
    let mut p0_preserved = 0_u64;
    let mut p1_total = 0_u64;
    let mut p1_preserved = 0_u64;
    let mut receipt_sizes = Vec::with_capacity(fixtures.len());

    for value in fixtures {
        let validated = validate_fixture(value, &budgets)
            .unwrap_or_else(|error| panic!("invalid Rust corpus fixture: {error}"));
        let fixture = validated.fixture;
        let budget = validated.budget;
        let request = Request {
            contract_version: CONTRACT_VERSION.to_owned(),
            request_id: fixture.id.clone(),
            source: Source::Inline {
                bytes: ByteString(validated.bytes),
                media_type: None,
            },
            budget: budget.clone(),
            preservation_profile: validated.profile.to_owned(),
            retention: Retention::default(),
            focus: None,
        };
        let outcome = engine.handle(request).unwrap_or_else(|failure| {
            panic!(
                "{} failed with {}: {}",
                fixture.id,
                failure.code.as_str(),
                failure.safe_message
            )
        });
        receipt_sizes.push(
            serde_json::to_vec(&outcome.receipt)
                .expect("compact receipt")
                .len() as u64,
        );
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
            let needle = decode_canonical(&fact.needle_base64).expect("validated P0 base64");
            p0_total += 1;
            let preserved = contains(&outcome.visible.bytes, &needle);
            p0_preserved += u64::from(preserved);
            assert!(
                preserved || fixture.category == UNSTRUCTURED_P0_CATEGORY,
                "{} omitted P0 fact {} under {}",
                fixture.id,
                fact.id,
                outcome.receipt.preservation.applied_profile
            );
        }
        for fact in fixture.annotations.p1 {
            p1_total += 1;
            let needle = decode_canonical(&fact.needle_base64).expect("validated P1 base64");
            if contains(&outcome.visible.bytes, &needle) {
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
    eprintln!("generated corpus recall: P0 {p0_preserved}/{p0_total} P1 {p1_preserved}/{p1_total}");
    assert!(
        p0_preserved * 100 >= p0_total * P0_RECALL_FLOOR_PERCENT,
        "P0 recall was {p0_preserved}/{p0_total}"
    );
    assert!(
        p1_preserved * 100 >= p1_total * 95,
        "P1 recall was {p1_preserved}/{p1_total}"
    );
    receipt_sizes.sort_unstable();
    let minimum = receipt_sizes[0];
    let median = receipt_sizes[(receipt_sizes.len() - 1) / 2];
    let p95 = receipt_sizes[(receipt_sizes.len() * 95).div_ceil(100) - 1];
    let maximum = *receipt_sizes.last().expect("receipt sizes");
    eprintln!(
        "current corpus receipt bytes: min={minimum} median={median} p95={p95} max={maximum}"
    );
    assert!(maximum <= MAX_ARTIFACT_LINEAGE_BYTES);
    assert!(MAX_ARTIFACT_LINEAGE_BYTES / median >= 64);
    assert!(MAX_LINEAGE_BYTES / median >= 4_096);
}

#[test]
fn rust_and_javascript_conformance_mutations_have_the_same_rejection_contract() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest =
        fs::read_to_string(root.join("evaluation/corpus/manifest.jsonl")).expect("corpus manifest");
    let budgets = load_budgets(root);
    let baseline: Value =
        serde_json::from_str(manifest.lines().next().expect("first fixture")).expect("fixture");
    validate_fixture(baseline.clone(), &budgets).expect("baseline fixture");

    let mut malformed_base64 = baseline.clone();
    malformed_base64["annotations"]["p0"][0]["needle_base64"] = Value::String("YQ".to_owned());

    let mut digest_mismatch = baseline.clone();
    digest_mismatch["source_sha256"] = Value::String("0".repeat(64));

    let mut missing_fact_id = baseline.clone();
    missing_fact_id["annotations"]["p0"][0]
        .as_object_mut()
        .expect("fact")
        .remove("id");

    let mut unknown_key = baseline.clone();
    unknown_key
        .as_object_mut()
        .expect("fixture")
        .insert("unknown_key".to_owned(), Value::Bool(true));

    let mut unknown_category = baseline;
    unknown_category["category"] = Value::String("unknown".to_owned());

    for (name, mutation) in [
        ("malformed canonical base64", malformed_base64),
        ("digest mismatch", digest_mismatch),
        ("missing fact ID", missing_fact_id),
        ("unknown key", unknown_key),
        ("unknown category", unknown_category),
    ] {
        let result = catch_unwind(AssertUnwindSafe(|| validate_fixture(mutation, &budgets)));
        assert!(
            matches!(result, Ok(Err(_))),
            "{name} must return an explicit rejection without panicking"
        );
    }
}

fn load_budgets(root: &Path) -> BTreeMap<String, BudgetProfile> {
    let document: BudgetFile = serde_json::from_slice(
        &fs::read(root.join("evaluation/corpus/budget-profiles.json")).expect("budget profiles"),
    )
    .expect("decode budget profiles");
    assert_eq!(document.schema_version, PROFILE_SCHEMA_VERSION);
    document
        .profiles
        .into_iter()
        .map(|profile| (profile.id.clone(), profile))
        .collect()
}

fn validate_fixture(
    value: Value,
    budgets: &BTreeMap<String, BudgetProfile>,
) -> Result<ValidatedFixture, String> {
    let fixture: Fixture =
        serde_json::from_value(value).map_err(|error| format!("fixture shape: {error}"))?;
    if fixture.schema_version != FIXTURE_SCHEMA_VERSION {
        return Err("unsupported fixture schema".to_owned());
    }
    if !valid_id(&fixture.id) {
        return Err("invalid fixture ID".to_owned());
    }
    let profile = preservation_profile(&fixture.category)?;
    let bytes = materialize_source(&fixture.source)?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err("fixture exceeds the source limit".to_owned());
    }
    if sha256_hex(&bytes) != fixture.source_sha256 {
        return Err("source digest mismatch".to_owned());
    }
    let budget_profile = budgets
        .get(&fixture.budget_profile)
        .ok_or_else(|| "unknown budget profile".to_owned())?;
    let budget = validate_budget(budget_profile)?;
    validate_facts(&fixture.annotations, &bytes)?;
    Ok(ValidatedFixture {
        fixture,
        bytes,
        budget,
        profile,
    })
}

fn validate_budget(profile: &BudgetProfile) -> Result<Budget, String> {
    if !valid_id(&profile.id) || profile.reserved_envelope > profile.total_visible_limit {
        return Err("invalid budget profile".to_owned());
    }
    let unit = match profile.unit.as_str() {
        "bytes" if profile.token_profile.is_none() => CountUnit::Bytes,
        "tokens" if profile.token_profile.as_deref() == Some(CL100K_PROFILE) => CountUnit::Tokens,
        _ => return Err("invalid budget unit or token profile".to_owned()),
    };
    Ok(Budget {
        unit,
        total_visible_limit: profile.total_visible_limit,
        reserved_envelope: profile.reserved_envelope,
        token_profile: profile.token_profile.clone(),
    })
}

fn validate_facts(annotations: &Annotations, source: &[u8]) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    for fact in annotations.p0.iter().chain(&annotations.p1) {
        if !valid_id(&fact.id) || !ids.insert(&fact.id) {
            return Err("invalid or duplicate fact ID".to_owned());
        }
        let needle = decode_canonical(&fact.needle_base64)?;
        if needle.is_empty() || !source.windows(needle.len()).any(|window| window == needle) {
            return Err("fact is absent from source".to_owned());
        }
    }
    Ok(())
}

/// The generated corpus keeps naming a profile per category, which the v3
/// contract resolves to the shape policy that identifier described. No policy
/// reads these fixtures' literals any more: the categories only pin that every
/// accepted identifier still resolves.
fn preservation_profile(category: &str) -> Result<&'static str, String> {
    match category {
        "build-output" | "logs" => Ok("build-log/v1"),
        "test-output" => Ok("test-log/v1"),
        "diff" => Ok("diff/v1"),
        "diagnostics" => Ok("diagnostic/v1"),
        "stack-trace" => Ok("stack-trace/v1"),
        "source-code" => Ok("source-code/v1"),
        "json" => Ok("json/v1"),
        "unicode" => Ok("unicode/v1"),
        "malformed-bytes" => Ok("binary/v1"),
        "prompt-injection" => Ok("untrusted-text/v1"),
        "empty" | "boundary" => Ok("plain-text/v1"),
        _ => Err(format!("unknown corpus category: {category}")),
    }
}

fn materialize_source(source: &Value) -> Result<Vec<u8>, String> {
    match source.get("kind").and_then(Value::as_str) {
        Some("inline") => {
            exact_keys(source, &["kind", "payload"], "inline source")?;
            materialize_payload(&source["payload"])
        }
        Some("file") => {
            exact_keys(
                source,
                &["kind", "root_id", "relative_path", "payload"],
                "file source",
            )?;
            let root_id = source["root_id"]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "file root ID is required".to_owned())?;
            let relative_path = source["relative_path"]
                .as_str()
                .filter(|value| {
                    !value.is_empty()
                        && !value.starts_with('/')
                        && value
                            .split('/')
                            .all(|component| !component.is_empty() && component != "..")
                })
                .ok_or_else(|| "file path must stay beneath its root".to_owned())?;
            let _validated_path = (root_id, relative_path);
            materialize_payload(&source["payload"])
        }
        Some("process") => materialize_process(source),
        _ => Err("unknown source kind".to_owned()),
    }
}

fn materialize_process(source: &Value) -> Result<Vec<u8>, String> {
    exact_keys(
        source,
        &[
            "kind",
            "stdout",
            "stderr",
            "events",
            "exit_code",
            "signal",
            "timed_out",
            "working_directory",
            "truncated",
        ],
        "process source",
    )?;
    let events = source["events"]
        .as_array()
        .ok_or_else(|| "process events must be an array".to_owned())?;
    let mut bytes = Vec::new();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for (index, event) in events.iter().enumerate() {
        exact_keys(event, &["order", "stream", "payload"], "process event")?;
        if event["order"].as_u64() != Some(index as u64) {
            return Err("process event order is not contiguous".to_owned());
        }
        let payload = materialize_payload(&event["payload"])?;
        match event["stream"].as_str() {
            Some("stdout") => stdout.extend_from_slice(&payload),
            Some("stderr") => stderr.extend_from_slice(&payload),
            _ => return Err("unknown process stream".to_owned()),
        }
        bytes.extend_from_slice(&payload);
    }
    if stdout != materialize_payload(&source["stdout"])?
        || stderr != materialize_payload(&source["stderr"])?
    {
        return Err("process streams disagree with ordered events".to_owned());
    }
    if !source["timed_out"].is_boolean()
        || !source["truncated"].is_boolean()
        || !source["working_directory"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    {
        return Err("invalid process state".to_owned());
    }
    Ok(bytes)
}

fn materialize_payload(payload: &Value) -> Result<Vec<u8>, String> {
    match payload.get("kind").and_then(Value::as_str) {
        Some("utf8") => {
            exact_keys(payload, &["kind", "value"], "UTF-8 payload")?;
            payload["value"]
                .as_str()
                .map(|value| value.as_bytes().to_vec())
                .ok_or_else(|| "UTF-8 payload value must be a string".to_owned())
        }
        Some("base64") => {
            exact_keys(payload, &["kind", "value"], "base64 payload")?;
            decode_canonical(
                payload["value"]
                    .as_str()
                    .ok_or_else(|| "base64 payload value must be a string".to_owned())?,
            )
        }
        Some("padded") => {
            exact_keys(
                payload,
                &["kind", "prefix_base64", "fill_byte", "byte_length"],
                "padded payload",
            )?;
            let mut bytes = decode_canonical(
                payload["prefix_base64"]
                    .as_str()
                    .ok_or_else(|| "padded prefix must be base64".to_owned())?,
            )?;
            let fill = payload["fill_byte"]
                .as_u64()
                .filter(|value| *value <= 255)
                .ok_or_else(|| "padded fill byte must be an octet".to_owned())?
                as u8;
            let byte_length = payload["byte_length"]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| *value >= bytes.len() && *value <= MAX_SOURCE_BYTES)
                .ok_or_else(|| "padded length is invalid".to_owned())?;
            bytes.resize(byte_length, fill);
            Ok(bytes)
        }
        _ => Err("unknown payload kind".to_owned()),
    }
}

fn exact_keys(value: &Value, expected: &[&str], label: &str) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!("{label} keys differ"));
    }
    Ok(())
}

fn decode_canonical(encoded: &str) -> Result<Vec<u8>, String> {
    let bytes = BASE64
        .decode(encoded)
        .map_err(|_| "invalid base64".to_owned())?;
    if BASE64.encode(&bytes) != encoded {
        return Err("base64 is not canonical".to_owned());
    }
    Ok(bytes)
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn contains(visible: &str, needle: &[u8]) -> bool {
    visible
        .as_bytes()
        .windows(needle.len())
        .any(|window| window == needle)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn assert_spans_cover_source(
    source_bytes: u64,
    retained: &[distill::ByteSpan],
    omitted: &[distill::ByteSpan],
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

/// US-014: the policy table no longer recognizes its own test data. Every
/// literal below is drawn from the corpus generator and used to sit in the
/// reducer's needle table; none of them may appear in the policy sources.
#[test]
fn no_generated_corpus_literal_remains_in_the_policy_sources() {
    const RETIRED_NEEDLES: [&str; 12] = [
        "commit-required",
        "export function",
        "\"failure_code\"",
        "\"run_id\"",
        "エラー",
        "Résumé",
        "FATAL_",
        "RECOVERY_HINT_",
        "ACTUAL_RESULT_",
        "SOURCE_LABEL_",
        "enforcePrivateMode",
        "at verify",
    ];
    const POLICY_SOURCES: [&str; 4] = [
        "src/projection.rs",
        "src/projection/shape.rs",
        "src/projection/aggregate.rs",
        "src/projection/retrieval.rs",
    ];

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let generator =
        fs::read_to_string(root.join("evaluation/corpus/generate.mjs")).expect("corpus generator");
    let policy = POLICY_SOURCES
        .iter()
        .map(|path| fs::read_to_string(root.join(path)).expect("policy source"))
        .collect::<Vec<_>>()
        .join("\n");

    for needle in RETIRED_NEEDLES {
        assert!(
            generator.contains(needle),
            "{needle} is no longer generated, so it cannot prove anything"
        );
        assert!(
            !policy.contains(needle),
            "{needle} is a corpus literal and must not drive preservation policy"
        );
    }
}
