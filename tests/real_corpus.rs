//! Evidence on the executed path: the real tool-output corpus projected through
//! `Engine::handle` at the budget the Codex hook runs, measured against the
//! frozen `evaluation/baseline/projection-baseline-v1.json` behavior.
//!
//! EP-002 proved the budget is spent. EP-004 moves the executed profile to
//! `auto/v1`, so the same measurements now qualify the shape-derived policy the
//! surfaces actually run, and the manifest labels qualify the classifier.

#![allow(clippy::expect_used, clippy::panic)]

use distill::{
    Budget, ByteSpan, ByteString, CL100K_PROFILE, CONTRACT_VERSION, CountUnit, Engine,
    EngineConfig, Fidelity, Request, Retention, Source,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

const REAL_CORPUS_SCHEMA_VERSION: &str = "distill.real-corpus/v1";
const EXECUTED_PROFILE: &str = distill::AUTO_PROFILE;

/// The executed Codex hook default: 2250 total visible tokens with a 450-token
/// reserved envelope, so 1800 tokens of payload.
const HOOK_TOTAL_VISIBLE_LIMIT: u64 = 2_250;
/// The adapter default. It is not slack: the envelope carries the payload as an
/// escaped JSON string, so its cost scales with the payload and reaches 621
/// tokens on this corpus.
const HOOK_RESERVED_ENVELOPE: u64 = 800;
const HOOK_PAYLOAD_LIMIT: u64 = HOOK_TOTAL_VISIBLE_LIMIT - HOOK_RESERVED_ENVELOPE;

/// EP-002 requires a median budget utilization of at least 85% on over-budget
/// real fixtures. The frozen baseline median is 0.7778%.
const UTILIZATION_FLOOR_BASIS_POINTS: u64 = 8_500;
const BASELINE_MEDIAN_UTILIZATION_BASIS_POINTS: u64 = 77;

/// US-011 requires the detected shape to match the manifest label for at least
/// 95% of the real corpus.
const SHAPE_AGREEMENT_FLOOR_PERCENT: usize = 95;

#[derive(Debug, Deserialize)]
struct RealFixture {
    schema_version: String,
    id: String,
    shape: String,
    command: Vec<String>,
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
        focus: None,
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

/// US-011: the shape `auto/v1` detects must agree with the label the corpus
/// manifest recorded from the capture command, for at least 95% of fixtures.
/// The receipt is the only observation point, which also proves it records the
/// policy that ran.
#[test]
fn detected_shapes_agree_with_the_real_corpus_labels() {
    let (_directory, engine) = engine();
    let corpus = real_corpus();
    let mut agreed = 0_usize;
    let mut disagreements = Vec::new();

    for (fixture, bytes) in &corpus {
        let outcome = engine
            .handle(request(&fixture.id, bytes.clone()))
            .unwrap_or_else(|failure| panic!("{} failed: {failure}", fixture.id));
        let preservation = &outcome.receipt.preservation;
        assert_eq!(preservation.profile, EXECUTED_PROFILE, "{}", fixture.id);
        let expected = format!("{}/v1", fixture.shape);
        if preservation.applied_profile == expected {
            agreed += 1;
        } else {
            disagreements.push(format!(
                "{}: labelled {expected}, detected {}",
                fixture.id, preservation.applied_profile
            ));
        }
    }

    eprintln!(
        "shape agreement {agreed}/{} disagreements={disagreements:?}",
        corpus.len()
    );
    assert!(
        agreed * 100 >= corpus.len() * SHAPE_AGREEMENT_FLOOR_PERCENT,
        "shape classification agreed on {agreed} of {} fixtures: {disagreements:?}",
        corpus.len()
    );
}

/// US-011: identical bytes always detect the same shape, so a projection cannot
/// change policy between two reads of the same observation.
#[test]
fn shape_detection_is_stable_across_repeated_classification() {
    let (_directory, engine) = engine();
    for (fixture, bytes) in real_corpus().into_iter().take(4) {
        let first = engine
            .handle(request(&fixture.id, bytes.clone()))
            .expect("first classification");
        let applied = first.receipt.preservation.applied_profile;
        for _ in 0..100 {
            let repeated = engine
                .handle(request(&fixture.id, bytes.clone()))
                .expect("repeated classification");
            assert_eq!(
                repeated.receipt.preservation.applied_profile, applied,
                "{}",
                fixture.id
            );
        }
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

const FOCUS_QUESTION_SCHEMA_VERSION: &str = "distill.real-corpus-focus/v1";

/// One question a caller could ask of a real fixture: what it is looking for,
/// and the literal line that answers it. The set is versioned data, so the gain
/// scored selection produces is measured rather than asserted.
#[derive(Debug, Deserialize)]
struct FocusQuestion {
    schema_version: String,
    id: String,
    fixture: String,
    focus: String,
    answer: String,
}

fn focus_questions() -> Vec<FocusQuestion> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("evaluation/corpus/real/focus-questions.jsonl");
    let questions = fs::read_to_string(path)
        .expect("focus questions")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let question: FocusQuestion = serde_json::from_str(line).expect("focus question");
            assert_eq!(question.schema_version, FOCUS_QUESTION_SCHEMA_VERSION);
            assert!(
                question.focus.len() <= distill::MAX_FOCUS_BYTES,
                "{} exceeds the contract focus bound",
                question.id
            );
            question
        })
        .collect::<Vec<_>>();
    assert!(questions.len() >= 12, "{} focus questions", questions.len());
    questions
}

/// US-016: on the questions the corpus carries, scored selection retains the
/// answer-bearing line in more cases than unscored selection does, at the same
/// budget, and it never loses an answer the unscored projection already held.
#[test]
fn scored_selection_answers_more_corpus_questions_than_unscored() {
    let (_directory, engine) = engine();
    let corpus = real_corpus();
    let mut unscored_hits = 0_usize;
    let mut scored_hits = 0_usize;
    let mut outcomes = Vec::new();

    for question in focus_questions() {
        let (_, bytes) = corpus
            .iter()
            .find(|(fixture, _)| fixture.id == question.fixture)
            .unwrap_or_else(|| panic!("{} names an unknown fixture", question.id));
        let body = std::str::from_utf8(bytes).expect("UTF-8 fixture");
        assert!(
            body.contains(&question.answer),
            "{} names a line the fixture does not carry",
            question.id
        );

        let unscored = engine
            .handle(request(&question.id, bytes.clone()))
            .unwrap_or_else(|failure| panic!("{} unscored: {failure}", question.id));
        let mut focused = request(&question.id, bytes.clone());
        focused.focus = Some(question.focus.clone());
        let scored = engine
            .handle(focused)
            .unwrap_or_else(|failure| panic!("{} scored: {failure}", question.id));

        assert!(scored.receipt.preservation.focus_applied, "{}", question.id);
        assert!(
            scored
                .receipt
                .visible_count
                .saturating_add(HOOK_RESERVED_ENVELOPE)
                <= HOOK_TOTAL_VISIBLE_LIMIT,
            "{} scored beyond the budget",
            question.id
        );

        let held = unscored.visible.bytes.contains(&question.answer);
        let found = scored.visible.bytes.contains(&question.answer);
        unscored_hits += usize::from(held);
        scored_hits += usize::from(found);
        outcomes.push(format!(
            "{}{} {}",
            if held { 'u' } else { '-' },
            if found { 'f' } else { '-' },
            question.id
        ));
        // A focus orders what the budget buys; it must not cost an answer the
        // same budget already delivered without one.
        assert!(
            found || !held,
            "{} lost an answer the unscored projection retained",
            question.id
        );
    }

    eprintln!("focus questions: unscored={unscored_hits} scored={scored_hits} {outcomes:?}");
    assert!(
        scored_hits > unscored_hits,
        "scored selection answered {scored_hits} questions against {unscored_hits} unscored"
    );
}

/// US-016 and US-017: the command that produced a fixture is exactly what the
/// Codex hook derives a focus from, so every over-budget fixture is projected
/// under one. The EP-002 budget guarantee, the receipt partition, and the span
/// ceiling must all survive intent-conditioned selection.
#[test]
fn a_derived_focus_keeps_the_executed_path_spending_its_budget() {
    let (_directory, engine) = engine();
    let mut utilization = Vec::new();

    for (fixture, bytes) in real_corpus() {
        let unfocused = engine
            .handle(request(&fixture.id, bytes.clone()))
            .unwrap_or_else(|failure| panic!("{} unfocused: {failure}", fixture.id));
        let mut focused = request(&fixture.id, bytes.clone());
        focused.focus = Some(distill_focus(&fixture.command));
        let outcome = engine
            .handle(focused)
            .unwrap_or_else(|failure| panic!("{} focused: {failure}", fixture.id));
        let receipt = &outcome.receipt;

        assert!(receipt.preservation.focus_applied, "{}", fixture.id);
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
        // A focus never changes which policy ran, nor what the observation is
        // accounted as: only the order the budget buys in.
        assert_eq!(
            receipt.preservation.applied_profile, unfocused.receipt.preservation.applied_profile,
            "{}",
            fixture.id
        );
        assert_eq!(
            receipt.original_count, unfocused.receipt.original_count,
            "{}",
            fixture.id
        );
        assert_eq!(
            receipt.fidelity, unfocused.receipt.fidelity,
            "{}",
            fixture.id
        );

        if receipt.original_count > HOOK_PAYLOAD_LIMIT {
            utilization.push((
                basis_points(receipt.visible_count, HOOK_PAYLOAD_LIMIT),
                fixture.id,
            ));
        }
    }

    utilization.sort_unstable();
    let median = utilization[(utilization.len() - 1) / 2].0;
    eprintln!(
        "focused executed path: min={} median={median}",
        utilization[0].0
    );
    assert!(
        median >= UTILIZATION_FLOOR_BASIS_POINTS,
        "focused median budget utilization was {}.{:02}%, below the {}% floor; lowest fixture {}",
        median / 100,
        median % 100,
        UTILIZATION_FLOOR_BASIS_POINTS / 100,
        utilization[0].1
    );
}

/// The focus the Codex hook would derive from the command that produced a
/// fixture, bounded exactly as the adapter bounds it.
fn distill_focus(command: &[String]) -> String {
    let mut focus = command.join(" ");
    if focus.len() > distill::MAX_FOCUS_BYTES {
        let mut boundary = distill::MAX_FOCUS_BYTES;
        while !focus.is_char_boundary(boundary) {
            boundary -= 1;
        }
        focus.truncate(boundary);
    }
    focus
}

/// The whole Codex `active` path, end to end, on every real fixture.
///
/// The engine-level tests above measure the payload. They cannot see the
/// envelope the adapter wraps around it, whose cost scales with the payload
/// once the projector fills its budget. Four source fixtures returned
/// `invariant_breach` instead of a projection before the reserve was corrected,
/// and no test observed it, because none of them ran the adapter.
#[test]
fn the_codex_active_path_projects_every_real_fixture() {
    let temp = tempfile::TempDir::new().expect("temp");
    let store = temp.path().join("store/artifacts.db");
    let store_directory = store.parent().expect("parent");
    fs::create_dir_all(store_directory).expect("store directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(store_directory, fs::Permissions::from_mode(0o700))
            .expect("private store directory");
    }
    let mut projected = 0_usize;
    for (fixture, bytes) in real_corpus() {
        let body = String::from_utf8_lossy(&bytes).into_owned();
        let event = serde_json::json!({
            "schema_version": "codex.post-tool-use/v2",
            "session_id": "real-corpus",
            "cwd": "/tmp",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": fixture.id,
            "tool_input": {"command": fixture.command.join(" ")},
            "tool_response": body,
        });
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_distill"))
            .args([
                "--store",
                &store.display().to_string(),
                "codex-hook",
                "--mode",
                "active",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .expect("stdin")
                    .write_all(event.to_string().as_bytes())?;
                child.wait_with_output()
            })
            .expect("codex hook");
        assert!(output.status.success(), "{} exited non-zero", fixture.id);
        let stdout = String::from_utf8(output.stdout).expect("hook stdout");
        if stdout.trim().is_empty() {
            // Exact fidelity: the observation already fits and passes through.
            continue;
        }
        let response: serde_json::Value = serde_json::from_str(&stdout).expect("hook response");
        let reason = response["reason"].as_str().expect("blocking feedback");
        let envelope: serde_json::Value = serde_json::from_str(reason).expect("envelope");
        assert!(
            envelope.get("error").is_none(),
            "{} returned {} instead of a projection",
            fixture.id,
            envelope["error"]["code"]
        );
        assert!(
            envelope.get("projection").is_some(),
            "{} has no payload",
            fixture.id
        );
        projected += 1;
    }
    assert!(
        projected > 0,
        "no real fixture exercised the projecting path"
    );
}

/// A diff is a list of independent edits, so a budget spent on its opening
/// hunks is a budget spent on one edit. Selection in source order retained 34%
/// of the bytes of the diff fixtures and 0 of their 10 answer lines, which is
/// worse than chance; ranking lines inside their hunk spreads the same budget.
#[test]
fn diff_selection_spreads_across_the_whole_observation() {
    let (_directory, engine) = engine();
    let mut measured = 0_usize;
    for (fixture, bytes) in real_corpus() {
        if fixture.shape != "unified-diff" {
            continue;
        }
        let outcome = engine
            .handle(request(&fixture.id, bytes.clone()))
            .unwrap_or_else(|failure| panic!("{} failed: {failure}", fixture.id));
        if outcome.receipt.fidelity == Fidelity::Exact {
            continue;
        }
        measured += 1;
        let source_length = bytes.len() as u64;
        let retained = &outcome.receipt.retained_spans;
        let last_end = retained.last().expect("retained spans").end;
        let covered_second_half = retained
            .iter()
            .any(|span| span.start >= source_length / 2 && span.end < source_length);
        assert!(
            covered_second_half,
            "{}: selection never reached past the midpoint (last span ends at {last_end} of {source_length})",
            fixture.id
        );
        assert!(
            retained.len() >= 4,
            "{}: {} retained spans, too concentrated to cover independent hunks",
            fixture.id,
            retained.len()
        );
    }
    assert!(
        measured >= 4,
        "{measured} over-budget diff fixtures measured"
    );
}
