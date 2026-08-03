use super::*;
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn bytes(limit: u64) -> Budget {
    Budget {
        unit: CountUnit::Bytes,
        total_visible_limit: limit,
        reserved_envelope: 0,
        token_profile: None,
    }
}

/// What the pre-expansion planner produced, kept only to measure against it.
struct Baseline {
    visible_count: u64,
    mandatory_fact_ids: Vec<String>,
}

/// Re-plays the planner as it stood before incremental counting: one full
/// render and one full tokenization per accepted candidate, no expansion, no
/// aggregation.
fn baseline_project(source: &[u8], budget: &Budget, profile: &str) -> (Baseline, usize) {
    let spec = ProjectionSpec::new(profile, budget)
        .expect("baseline spec")
        .without_aggregation();
    let text = std::str::from_utf8(source).expect("baseline text");
    let spec = spec.resolved(Some(text));
    let original_count = count(text, budget).expect("baseline original count");
    let payload_limit = budget.total_visible_limit - budget.reserved_envelope;
    assert!(original_count > payload_limit);

    let empty = Aggregation::default();
    let analysis = analyze(source, text, spec).expect("baseline analysis");
    let mandatory = analysis.mandatory;
    let mut evaluations = 1;
    let mandatory_visible = render(source, &mandatory, &empty).expect("baseline mandatory render");
    assert!(count(&mandatory_visible, budget).expect("baseline mandatory count") <= payload_limit);

    let mut retained = mandatory.clone();
    for candidate in analysis.candidates {
        let mut proposed = retained.clone();
        proposed.push(candidate);
        normalize_spans(&mut proposed);
        evaluations += 1;
        if count(
            &render(source, &proposed, &empty).expect("baseline proposal render"),
            budget,
        )
        .expect("baseline proposal count")
            <= payload_limit
        {
            retained = proposed;
        }
    }
    if retained.is_empty() && payload_limit > 0 {
        let prefix_end =
            fitting_prefix(text, budget, payload_limit).expect("baseline fitting prefix");
        if prefix_end > 0 {
            retained.push(ByteSpan {
                start: 0,
                end: prefix_end as u64,
            });
        }
    }
    normalize_spans(&mut retained);
    evaluations += 1;
    let visible = render(source, &retained, &empty).expect("baseline final render");
    let visible_count = count(&visible, budget).expect("baseline final count");
    (
        Baseline {
            visible_count,
            mandatory_fact_ids: mandatory
                .iter()
                .map(|span| fact_id(spec.applied_profile(), *span))
                .collect(),
        },
        evaluations,
    )
}

fn p95(mut durations: Vec<Duration>) -> Duration {
    durations.sort_unstable();
    durations[18]
}

#[test]
fn exact_text_and_cl100k_accounting_are_exact() {
    let exact = project(b"hello world", &bytes(11), "plain-text/v1").expect("exact");
    assert_eq!(exact.visible, "hello world");
    assert_eq!(exact.original_count, 11);
    assert_eq!(exact.fidelity, Fidelity::Exact);

    let token_budget = Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: 2,
        reserved_envelope: 0,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    };
    let token = project(b"hello world", &token_budget, "plain-text/v1").expect("tokens");
    assert_eq!(token.original_count, 2);
    assert_eq!(token.visible_count, 2);
    assert_eq!(token.visible, "hello world");
}

/// US-013: every shape policy keeps the fragment that carries the answer of
/// that shape, under a budget far too small to keep the observation. None of
/// these markers is a literal drawn from a fixture: they are the grammar the
/// tool families emit.
#[test]
fn every_shape_policy_preserves_the_fragment_that_carries_its_answer() {
    let cases = [
        (
            "build-output/v1",
            "error[E0308]: mismatched types",
            "warning: unused variable `budget`",
        ),
        (
            "typecheck-lint/v1",
            "src/main.rs:7:22: error[E0425]: cannot find function `missing_helper`",
            "warning: this expression creates a reference",
        ),
        (
            "test-output/v1",
            "test budget::spends_its_payload ... FAILED",
            "test result: FAILED. 3 passed; 1 failed",
        ),
        (
            "stack-trace/v1",
            "TypeError: budget must be a number, received string",
            "at loadBudget (/home/dev/work/js/throwing.js:3:15)",
        ),
        (
            "unified-diff/v1",
            "@@ -1,2 +1,3 @@",
            "+    enforce_private_mode(root, 0o700);",
        ),
        (
            "api-json/v1",
            "\"failure_code\": \"store_corrupt\",",
            "\"run_id\": \"run-1\",",
        ),
        (
            "source-file/v1",
            "pub fn verify(receipt: &Receipt) -> bool {",
            "use crate::artifact::Receipt;",
        ),
    ];
    for (profile, answer, context) in cases {
        let source = format!(
            "header\n{}{context}\n{answer}\ntail\n",
            "ordinary output line\n".repeat(80),
        );
        let outcome = project(source.as_bytes(), &bytes(220), profile).expect(profile);
        assert!(
            outcome.visible.contains(answer),
            "{profile} omitted the answer-bearing line"
        );
        assert!(
            outcome.visible.contains(context),
            "{profile} omitted its supporting line"
        );
        assert!(outcome.visible_count <= 220);
        assert_eq!(outcome.fidelity, Fidelity::Extractive);
        assert_eq!(outcome.applied_profile, profile);
    }
}

#[test]
fn impossible_mandatory_content_fails_closed() {
    let source = b"noise\nerror[E0123]: mandatory compiler diagnostic\nnoise\n";
    let failure = project(source, &bytes(4), "build-output/v1").expect_err("unsatisfiable");
    assert_eq!(failure.code, FailureCode::BudgetUnsatisfiable);
}

#[test]
fn malformed_bytes_are_encoded_without_losing_ascii_facts() {
    let source = b"\xffFATAL_MALFORMED_01\nRECOVERY_HINT_01\0";
    let outcome = project(source, &bytes(256), "binary/v1").expect("encoded");
    assert_eq!(outcome.fidelity, Fidelity::Encoded);
    assert!(outcome.visible.contains("FATAL_MALFORMED_01"));
    assert!(outcome.visible.contains("RECOVERY_HINT_01"));
    assert!(outcome.visible.contains("\\xff"));
    assert!(outcome.visible.contains("\\x00"));
}

#[test]
fn oversized_binary_without_facts_becomes_metadata_only() {
    let source = vec![0xff; 1_024];
    let outcome = project(&source, &bytes(64), "none/v1").expect("metadata");
    assert_eq!(outcome.fidelity, Fidelity::MetadataOnly);
    assert_eq!(outcome.visible, "[binary source: 1024 bytes]");
    assert_eq!(
        outcome.omitted_spans,
        vec![ByteSpan {
            start: 0,
            end: 1_024
        }]
    );
}

#[test]
fn zero_and_reserved_budgets_are_enforced() {
    let empty = project(b"", &bytes(0), "plain-text/v1").expect("empty");
    assert_eq!(empty.visible, "");

    assert_eq!(
        project(b"x", &bytes(0), "plain-text/v1")
            .expect_err("nonempty zero budget")
            .code,
        FailureCode::BudgetUnsatisfiable
    );

    let invalid = Budget {
        unit: CountUnit::Bytes,
        total_visible_limit: 4,
        reserved_envelope: 5,
        token_profile: None,
    };
    assert_eq!(
        project(b"x", &invalid, "plain-text/v1")
            .expect_err("invalid envelope")
            .code,
        FailureCode::InvalidRequest
    );
}

#[test]
fn unknown_profiles_and_tokenizers_are_distinct() {
    assert_eq!(
        project(b"x", &bytes(1), "unknown/v1")
            .expect_err("profile")
            .code,
        FailureCode::InvalidRequest
    );
    let unknown = Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: 1,
        reserved_envelope: 0,
        token_profile: Some("unknown@v1".to_owned()),
    };
    assert_eq!(
        project(b"x", &unknown, "plain-text/v1")
            .expect_err("tokenizer")
            .code,
        FailureCode::TokenProfileUnsupported
    );
    let mislabeled = Budget {
        unit: CountUnit::Bytes,
        total_visible_limit: 1,
        reserved_envelope: 0,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    };
    assert_eq!(
        project(b"x", &mislabeled, "plain-text/v1")
            .expect_err("mislabeled")
            .code,
        FailureCode::InvalidRequest
    );
}

#[test]
fn spans_cover_source_once_and_output_is_deterministic() {
    let source = b"head\nnoise\nERROR E1: critical\nnoise\nwarning W1: useful\nnoise\ntail\n";
    let first = project(source, &bytes(50), "build-output/v1").expect("projection");
    for _ in 0..100 {
        let repeated = project(source, &bytes(50), "build-output/v1").expect("repeat");
        assert_eq!(repeated.visible, first.visible);
        assert_eq!(repeated.retained_spans, first.retained_spans);
        assert_eq!(repeated.omitted_spans, first.omitted_spans);
        assert_eq!(repeated.visible_count, first.visible_count);
    }
    let mut coverage = vec![0_u8; source.len()];
    for span in first
        .retained_spans
        .iter()
        .chain(first.omitted_spans.iter())
    {
        for byte in &mut coverage[span.start as usize..span.end as usize] {
            *byte += 1;
        }
    }
    assert!(coverage.iter().all(|count| *count == 1));
}

#[test]
fn byte_budget_property_holds_across_boundaries() {
    for source_len in [0, 1, 2, 31, 32, 33, 255, 256, 257, 4_096] {
        let source = "é".repeat(source_len);
        for limit in [0, 1, 2, 7, 31, 32, 128, 1_024] {
            let result = project(source.as_bytes(), &bytes(limit), "plain-text/v1");
            if !source.is_empty() && limit == 0 {
                assert_eq!(
                    result.expect_err("zero payload").code,
                    FailureCode::BudgetUnsatisfiable
                );
                continue;
            }
            let outcome = result.expect("bounded projection");
            assert!(outcome.visible.len() as u64 <= limit);
            assert!(std::str::from_utf8(outcome.visible.as_bytes()).is_ok());
        }
    }
}

#[test]
fn newline_dense_input_has_bounded_reducer_work_and_receipt_spans() {
    let source = "\n".repeat(10 * 1024 * 1024);
    let outcome = project(source.as_bytes(), &bytes(128), "plain-text/v1")
        .expect("bounded newline projection");
    assert!(outcome.visible_count <= 128);
    assert!(outcome.retained_spans.len() <= 2);

    let mandatory = "error: E1\n".repeat(MAX_REDUCER_SPANS + 1);
    assert_eq!(
        project(mandatory.as_bytes(), &bytes(64), "build-output/v1")
            .expect_err("mandatory span cap")
            .code,
        FailureCode::ResourceExhausted
    );
}

#[test]
fn optional_candidates_leave_room_for_boundary_spans() {
    let mut source = String::from("first boundary\nordinary separator\n");
    let mut selected_bytes = "first boundary\n".len() + "last boundary\n".len();
    for index in 0..MAX_REDUCER_SPANS {
        let candidate = format!("warning: W{index:03}\n");
        selected_bytes += candidate.len();
        source.push_str(&candidate);
        source.push_str("ordinary separator\n");
    }
    source.push_str("last boundary\n");

    let outcome = project(
        source.as_bytes(),
        &bytes(selected_bytes as u64),
        "build-output/v1",
    )
    .expect("bounded optional projection");

    assert_eq!(outcome.fidelity, Fidelity::Extractive);
    assert!(outcome.retained_spans.len() <= MAX_RECEIPT_SPANS);
    assert!(outcome.omitted_spans.len() <= MAX_RECEIPT_SPANS);
}

#[test]
fn helpers_reject_invalid_spans_and_normalize_overlap() {
    let failure = render(
        b"abc",
        &[ByteSpan { start: 1, end: 9 }],
        &Aggregation::default(),
    )
    .expect_err("out-of-range span");
    assert_eq!(failure.code, FailureCode::InvariantBreach);

    let mut spans = vec![
        ByteSpan { start: 3, end: 5 },
        ByteSpan { start: 0, end: 4 },
        ByteSpan { start: 8, end: 9 },
    ];
    normalize_spans(&mut spans);
    assert_eq!(
        spans,
        vec![ByteSpan { start: 0, end: 5 }, ByteSpan { start: 8, end: 9 }]
    );
}

#[test]
fn policy_heavy_planning_counts_incrementally_within_the_render_bound() {
    let mut source = String::from("error[E1]: mandatory fact\n");
    for index in 0..MAX_REDUCER_SPANS {
        source.push_str(&format!("warning: W{index:03} optional fact\n"));
        source.push_str("ordinary build output without policy facts\n");
    }
    while source.len() < 1024 * 1024 {
        source.push_str("ordinary build output padding\n");
    }
    source.truncate(1024 * 1024);
    let budget = Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: 8_192,
        reserved_envelope: 0,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    };

    let (baseline, baseline_evaluations) =
        baseline_project(source.as_bytes(), &budget, "build-output/v1");
    let mut metrics = PlanningMetrics::default();
    let planned = project_with_metrics(source.as_bytes(), &budget, "build-output/v1", &mut metrics)
        .expect("planned projection");
    assert_eq!(planned.mandatory_fact_ids, baseline.mandatory_fact_ids);
    assert!(planned.visible.contains("error[E1]: mandatory fact"));
    assert!(planned.visible.contains("warning: W000 optional fact"));

    // The count carried through planning is the exact count of what is rendered.
    assert_eq!(
        planned.visible_count,
        count(&planned.visible, &budget).expect("planned payload count")
    );
    // Accepting one candidate at a time costs full renders proportional to the
    // candidate count; the incremental ledger costs a documented constant.
    assert!(
        baseline_evaluations > 200,
        "{baseline_evaluations} naive evaluations do not exercise the bound"
    );
    assert!(
        metrics.full_render_count_evaluations <= MAX_PLAN_FULL_RENDER_COUNTS,
        "{} full-render tokenizations exceed the {MAX_PLAN_FULL_RENDER_COUNTS} bound",
        metrics.full_render_count_evaluations
    );
    // Expansion spends the budget the unexpanded selection left unused.
    assert!(planned.visible_count > baseline.visible_count);
    assert!(
        planned.visible_count * 100 >= 8_192 * 85,
        "planned projection spent only {} of 8192 payload tokens",
        planned.visible_count
    );

    for _ in 0..2 {
        black_box(baseline_project(
            black_box(source.as_bytes()),
            black_box(&budget),
            "build-output/v1",
        ));
        let mut warmup_metrics = PlanningMetrics::default();
        black_box(
            project_with_metrics(
                black_box(source.as_bytes()),
                black_box(&budget),
                "build-output/v1",
                &mut warmup_metrics,
            )
            .expect("planned warm-up"),
        );
    }
    let baseline_times = (0..20)
        .map(|_| {
            let started = Instant::now();
            black_box(baseline_project(
                black_box(source.as_bytes()),
                black_box(&budget),
                "build-output/v1",
            ));
            started.elapsed()
        })
        .collect();
    let planned_times = (0..20)
        .map(|_| {
            let started = Instant::now();
            let mut run_metrics = PlanningMetrics::default();
            black_box(
                project_with_metrics(
                    black_box(source.as_bytes()),
                    black_box(&budget),
                    "build-output/v1",
                    &mut run_metrics,
                )
                .expect("measured planned projection"),
            );
            started.elapsed()
        })
        .collect();
    let baseline_p95 = p95(baseline_times);
    let planned_p95 = p95(planned_times);
    assert!(
        planned_p95.as_nanos() * 100 <= baseline_p95.as_nanos() * 105,
        "planned P95 {planned_p95:?} regressed beyond baseline P95 {baseline_p95:?}"
    );
}

fn token_spec(total: u64, reserved: u64) -> ProjectionSpec {
    ProjectionSpec::new(
        "plain-text/v1",
        &Budget {
            unit: CountUnit::Tokens,
            total_visible_limit: total,
            reserved_envelope: reserved,
            token_profile: Some(CL100K_PROFILE.to_owned()),
        },
    )
    .expect("token spec")
}

/// The ledger sums span counts instead of tokenizing the render, which is exact
/// only on the offsets `additive_token_offset` accepts. Splitting every real
/// fixture on exactly those offsets must reproduce its whole-text count.
#[test]
fn additive_offsets_partition_the_real_corpus_without_changing_its_count() {
    let spec = token_spec(1_000_000, 0);
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("evaluation/corpus/real/fixtures");
    let mut inspected = 0;
    for entry in std::fs::read_dir(&directory).expect("real corpus fixtures") {
        let path = entry.expect("fixture entry").path();
        let bytes = std::fs::read(&path).expect("fixture bytes");
        let text = std::str::from_utf8(&bytes).expect("fixture is UTF-8");

        let mut cuts = vec![0_usize];
        for (index, byte) in bytes.iter().enumerate() {
            let offset = index + 1;
            if *byte == b'\n' && offset < bytes.len() && additive_token_offset(text, offset) {
                cuts.push(offset);
            }
        }
        cuts.push(bytes.len());
        let summed = cuts
            .windows(2)
            .map(|pair| spec.count(&text[pair[0]..pair[1]]))
            .sum::<u64>();
        assert_eq!(
            summed,
            spec.count(text),
            "{} does not count additively across its additive offsets",
            path.display()
        );
        inspected += 1;
    }
    assert!(inspected >= 40, "{inspected} real fixtures inspected");
}

#[test]
fn additive_offsets_hold_across_line_shapes() {
    let spec = token_spec(1_000_000, 0);
    let additive = [
        ("a\n", "b\n"),
        ("a\n", "  b\n"),
        ("a\n", "\tb\n"),
        (");\n", "}\n"),
        ("   \n", "b\n"),
        ("a\r\n", "b\r\n"),
        ("a\n", "é\n"),
        ("héllo wörld\n", "  → ok\n"),
        ("-----\n", "x\n"),
        ("\n", "x\n"),
        ("a\n\n\n", "x\n"),
        ("0123\n", "4567\n"),
        ("f(\n", ")\n"),
        ("\"json\": [\n", "  1,\n"),
    ];
    for (left, right) in additive {
        let joined = format!("{left}{right}");
        assert!(additive_token_offset(&joined, left.len()));
        assert_eq!(
            spec.count(&joined),
            spec.count(left) + spec.count(right),
            "{left:?} ++ {right:?} does not count additively"
        );
    }

    // A following line that reaches a line break before any other character
    // merges with the break that precedes it, so those offsets are rejected.
    // Rejection is conservative: the joined count is never larger than the sum,
    // and the blank line proves the merge is real.
    for (left, right) in [("a\n", "\n"), ("a\n", " \n"), ("a\n", "\r\n")] {
        let joined = format!("{left}{right}");
        assert!(!additive_token_offset(&joined, left.len()));
        assert!(spec.count(&joined) <= spec.count(left) + spec.count(right));
    }
    assert!(spec.count("a\n\n") < spec.count("a\n") + spec.count("\n"));
}

/// US-004: a plan whose carried count disagrees with the payload it renders is
/// a broken ledger, and must fail closed instead of returning that payload.
#[test]
fn a_disagreeing_planned_count_fails_closed() {
    let source = "let value = 1;\n".repeat(400);
    let budget = Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: 225,
        reserved_envelope: 45,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    };
    project(source.as_bytes(), &budget, "plain-text/v1").expect("agreeing plan");

    PLAN_COUNT_BIAS.with(|bias| bias.set(1));
    let failure = project(source.as_bytes(), &budget, "plain-text/v1");
    PLAN_COUNT_BIAS.with(|bias| bias.set(0));
    assert_eq!(
        failure.expect_err("disagreeing plan").code,
        FailureCode::InvariantBreach
    );
}

fn real_fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("evaluation/corpus/real/fixtures")
            .join(name),
    )
    .expect("real corpus fixture")
}

/// US-005: expansion must spend the payload budget on the two fixtures the PRD
/// measures, whose baseline utilization is 0.39% and 2.4%.
#[test]
fn expansion_spends_the_payload_budget_on_the_measured_fixtures() {
    let budget = Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: 2_250,
        reserved_envelope: 450,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    };
    for (name, baseline_visible_count) in
        [("source-artifact-rs.txt", 7), ("log-git-log-stat.txt", 44)]
    {
        let source = real_fixture(name);
        let outcome = project(&source, &budget, "plain-text/v1").expect(name);
        assert_eq!(outcome.fidelity, Fidelity::Extractive);
        assert!(outcome.visible_count > baseline_visible_count);
        assert!(
            outcome.visible_count * 100 >= 1_800 * 85,
            "{name} spent {} of 1800 payload tokens",
            outcome.visible_count
        );
        assert!(outcome.visible_count <= 1_800);
        assert_eq!(
            outcome.visible_count,
            count(&outcome.visible, &budget).expect("rendered payload count")
        );
        assert!(std::str::from_utf8(outcome.visible.as_bytes()).is_ok());
        assert!(outcome.retained_spans.len() <= MAX_RECEIPT_SPANS);
        assert!(outcome.omitted_spans.len() <= MAX_RECEIPT_SPANS);
    }
}

/// US-005: expansion keeps the head and the tail of the observation, grows only
/// on line boundaries, and never displaces a mandatory span.
#[test]
fn expansion_grows_line_groups_around_retained_content() {
    let mut source = String::from("first line\n");
    for index in 0..400 {
        source.push_str(&format!("filler line {index:03}\n"));
        if index == 200 {
            source.push_str("error[E1]: mandatory fact\n\n\n");
        }
    }
    source.push_str("last line\n");
    let budget = Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: 225,
        reserved_envelope: 45,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    };
    let outcome =
        project(source.as_bytes(), &budget, "build-output/v1").expect("expanded projection");

    assert!(outcome.visible.contains("error[E1]: mandatory fact"));
    assert!(outcome.visible.starts_with("first line\n"));
    assert!(outcome.visible.ends_with("last line\n"));
    assert!(
        outcome.visible_count * 100 >= 180 * 85,
        "expansion spent {} of 180 payload tokens",
        outcome.visible_count
    );
    // Growth advances a whole line group at a time, so no retained span ever
    // starts or ends inside a line.
    for span in &outcome.retained_spans {
        if span.start > 0 {
            assert_eq!(source.as_bytes()[span.start as usize - 1], b'\n');
        }
        if (span.end as usize) < source.len() {
            assert_eq!(source.as_bytes()[span.end as usize - 1], b'\n');
        }
    }

    for _ in 0..100 {
        let repeated = project(source.as_bytes(), &budget, "build-output/v1").expect("repeat");
        assert_eq!(repeated.visible, outcome.visible);
        assert_eq!(repeated.retained_spans, outcome.retained_spans);
        assert_eq!(repeated.omitted_spans, outcome.omitted_spans);
        assert_eq!(repeated.visible_count, outcome.visible_count);
    }
}

/// US-005: a first line that alone exceeds the payload budget leaves selection
/// empty, so the prefix fallback still applies and still fills the budget.
#[test]
fn an_oversized_first_line_keeps_the_prefix_fallback() {
    let source = format!("{}\n", "minified ".repeat(4_000));
    let budget = Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: 225,
        reserved_envelope: 45,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    };
    let outcome = project(source.as_bytes(), &budget, "plain-text/v1").expect("prefix fallback");
    assert_eq!(outcome.fidelity, Fidelity::Extractive);
    assert_eq!(outcome.retained_spans.len(), 1);
    assert_eq!(outcome.retained_spans[0].start, 0);
    assert!(
        outcome.visible_count * 100 >= 180 * 85,
        "prefix fallback spent {} of 180 payload tokens",
        outcome.visible_count
    );
    assert!(outcome.visible_count <= 180);
}

/// US-007: a selection whose plan fragments inside many regions still returns a
/// receipt both persisted partitions can hold, in original source offsets.
#[test]
fn a_fragmented_selection_holds_the_receipt_span_ceiling() {
    let mut source = String::new();
    for block in 0..crate::contract::MAX_SELECTOR_MATCHES {
        for filler in 0..20 {
            source.push_str(&format!("filler {block:02}-{filler:02} separator line\n"));
        }
        source.push_str(&format!("SELECT_ME block {block:02}\n"));
        for index in 0..32 {
            source.push_str(&format!("warning: W{block:02}{index:02} optional fact\n"));
            source.push_str("ordinary build output without policy facts\n");
        }
    }

    let spec = ProjectionSpec::new("build-output/v1", &bytes(8_192)).expect("selection spec");
    let outcome = project_selection(
        source.as_bytes(),
        &Selection::Pattern {
            pattern: "SELECT_ME".to_owned(),
            before_lines: 16,
            after_lines: 16,
            max_matches: crate::contract::MAX_SELECTOR_MATCHES,
        },
        spec,
    )
    .expect("fragmented selection");

    assert_eq!(outcome.fidelity, Fidelity::Extractive);
    assert!(outcome.visible_count <= 8_192);
    assert_eq!(outcome.original_count, source.len() as u64);
    assert!(
        outcome.retained_spans.len() > 1,
        "selection did not fragment across regions"
    );
    assert!(outcome.retained_spans.len() <= MAX_RECEIPT_SPANS);
    assert!(outcome.omitted_spans.len() <= MAX_RECEIPT_SPANS);

    // Retained and omitted spans still partition the whole committed source.
    let mut coverage = vec![0_u8; source.len()];
    for span in outcome
        .retained_spans
        .iter()
        .chain(outcome.omitted_spans.iter())
    {
        for byte in &mut coverage[span.start as usize..span.end as usize] {
            *byte += 1;
        }
    }
    assert!(coverage.iter().all(|count| *count == 1));
    // The visible payload is exactly the concatenation of the retained spans.
    assert_eq!(
        outcome.visible,
        render(
            source.as_bytes(),
            &outcome.retained_spans,
            &Aggregation::default()
        )
        .expect("retained render")
    );
}

/// US-006: at the receipt span ceiling a candidate is joined to the nearest
/// retained span, in either direction, so no fragment is dropped.
#[test]
fn bridging_joins_a_candidate_to_its_nearest_retained_neighbour() {
    let spec = token_spec(1_000_000, 0);
    let text = "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\n";
    let plan = SpanPlan::new(
        spec,
        text,
        &[
            ByteSpan { start: 6, end: 12 },
            ByteSpan { start: 20, end: 26 },
        ],
        &Aggregation::default(),
    );

    // A candidate after both spans bridges back from the nearest one.
    assert_eq!(
        bridge(&plan, ByteSpan { start: 31, end: 39 }),
        Some(ByteSpan { start: 26, end: 39 })
    );
    // A candidate before both bridges forward to the nearest one.
    assert_eq!(
        bridge(&plan, ByteSpan { start: 0, end: 6 }),
        Some(ByteSpan { start: 0, end: 6 })
    );
    // Ties resolve to the preceding neighbour.
    assert_eq!(
        bridge(&plan, ByteSpan { start: 13, end: 19 }),
        Some(ByteSpan { start: 12, end: 19 })
    );
    // Bridging a candidate keeps the retained span count unchanged.
    let bridged = plan.with_candidate(
        spec,
        text,
        bridge(&plan, ByteSpan { start: 31, end: 39 }).expect("bridge"),
        &Aggregation::default(),
    );
    assert_eq!(bridged.spans.len(), plan.spans.len());
    assert_eq!(
        bridge(&SpanPlan::default(), ByteSpan { start: 0, end: 6 }),
        None
    );
}

/// US-006: selection at the receipt span ceiling merges fragments rather than
/// dropping them, and expansion never returns an unpersistable receipt.
#[test]
fn receipt_span_ceiling_merges_fragments_instead_of_dropping_them() {
    let mut source = String::from("first boundary\n");
    for index in 0..MAX_REDUCER_SPANS {
        source.push_str(&format!("warning: W{index:03}\n"));
        source.push_str("ordinary separator ordinary separator ordinary separator\n");
    }
    source.push_str("last boundary\n");

    let budget = Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: 2_250,
        reserved_envelope: 450,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    };
    let outcome =
        project(source.as_bytes(), &budget, "build-output/v1").expect("ceiling projection");

    assert!(outcome.retained_spans.len() <= MAX_RECEIPT_SPANS);
    assert!(outcome.omitted_spans.len() <= MAX_RECEIPT_SPANS);
    assert!(outcome.visible_count <= 1_800);
    assert!(outcome.visible.contains("warning: W000"));

    // The partition stays exact at the ceiling.
    let mut coverage = vec![0_u8; source.len()];
    for span in outcome
        .retained_spans
        .iter()
        .chain(outcome.omitted_spans.iter())
    {
        for byte in &mut coverage[span.start as usize..span.end as usize] {
            *byte += 1;
        }
    }
    assert!(coverage.iter().all(|count| *count == 1));
}

fn project_without_aggregation(
    source: &[u8],
    budget: &Budget,
    profile: &str,
) -> Result<Projection, Failure> {
    ProjectionSpec::new(profile, budget)
        .map(ProjectionSpec::without_aggregation)
        .and_then(|spec| project_validated(source, spec))
}

/// US-011: classification decides from a bounded prefix, so bytes past that
/// prefix cannot change the policy, and a 10 MiB observation costs the same
/// decision as a small one.
#[test]
fn shape_detection_reads_a_bounded_prefix_within_its_deadline() {
    let mut diff = String::from("diff --git a/src/main.rs b/src/main.rs\n@@ -1,2 +1,3 @@\n");
    while diff.len() < 10 * 1024 * 1024 {
        diff.push_str(" context line that never changes the decision\n");
    }
    assert_eq!(shape::detect(&diff), Shape::UnifiedDiff);

    // A trailer past the inspected prefix cannot flip the decision.
    let mut disguised = "ordinary log line without structure\n".repeat(4_096);
    assert!(disguised.len() > shape::CLASSIFICATION_PREFIX_BYTES);
    disguised.push_str("diff --git a/src/main.rs b/src/main.rs\n@@ -1,2 +1,3 @@\n");
    assert_eq!(shape::detect(&disguised), Shape::TerminalLog);

    for _ in 0..2 {
        black_box(shape::detect(black_box(&diff)));
    }
    let elapsed = (0..20)
        .map(|_| {
            let started = Instant::now();
            black_box(shape::detect(black_box(&diff)));
            started.elapsed()
        })
        .collect::<Vec<_>>();
    let measured = p95(elapsed);
    assert!(
        measured <= Duration::from_millis(SHAPE_DETECTION_DEADLINE_MS),
        "classification P95 was {measured:?} on a 10 MiB source"
    );
}

/// The documented classification deadline, measured on the unoptimized test
/// profile so the gate bounds the worst build the project ships from.
const SHAPE_DETECTION_DEADLINE_MS: u64 = 50;

/// US-011: an input that matches no shape falls back to the line-structured
/// policy instead of failing, and a non-UTF-8 source keeps its binary handling.
#[test]
fn unrecognized_and_binary_sources_keep_their_existing_handling() {
    let prose = "the quick brown fox jumps over the lazy dog\n".repeat(8);
    assert_eq!(shape::detect(&prose), Shape::TerminalLog);
    let ambiguous = project(prose.as_bytes(), &bytes(64), AUTO_PROFILE).expect("ambiguous");
    assert_eq!(ambiguous.applied_profile, "terminal-log/v1");
    assert_eq!(ambiguous.fidelity, Fidelity::Extractive);

    let binary = project(&[0xff_u8; 1_024], &bytes(64), AUTO_PROFILE).expect("binary");
    assert_eq!(binary.fidelity, Fidelity::MetadataOnly);
    assert_eq!(binary.applied_profile, "terminal-log/v1");
    assert_eq!(binary.visible, "[binary source: 1024 bytes]");
}

/// Forty subjects that share no leading token, so the lines built from them are
/// distinct observations rather than one template with a variable in it.
const SUBJECTS: [&str; 40] = [
    "acquisition",
    "artifact",
    "backoff",
    "budget",
    "cache",
    "commit",
    "counter",
    "digest",
    "envelope",
    "expansion",
    "fidelity",
    "frontier",
    "gate",
    "handle",
    "identity",
    "journal",
    "kernel",
    "lineage",
    "manifest",
    "needle",
    "omission",
    "partition",
    "quota",
    "receipt",
    "retention",
    "scheduler",
    "selector",
    "shape",
    "store",
    "template",
    "token",
    "trace",
    "unit",
    "validator",
    "verdict",
    "walker",
    "window",
    "worker",
    "yield",
    "zone",
];

/// A build log whose progress lines differ only in their variable literals,
/// followed by distinct observations that the budget can only reach once the
/// redundancy stops paying for itself.
fn redundant_build_output(distinct: usize) -> String {
    let mut source = String::from("build started\n");
    for index in 0..60 {
        source.push_str(&format!("   Compiling crate_name v0.1.{index}\n"));
    }
    for subject in SUBJECTS.iter().take(distinct) {
        source.push_str(&format!("{subject}: reported an unrepeated condition\n"));
    }
    source.push_str("build finished\n");
    source
}

/// US-012: redundant lines collapse to one representative with the count of
/// what was dropped, and the budget that frees buys distinct content.
#[test]
fn aggregation_collapses_repeated_lines_and_buys_distinct_content() {
    let source = redundant_build_output(40);
    let budget = bytes(700);
    let aggregated = project(source.as_bytes(), &budget, "build-output/v1").expect("aggregated");
    let verbatim =
        project_without_aggregation(source.as_bytes(), &budget, "build-output/v1").expect("plain");

    // The representative survives; the repeats become one stated annotation.
    assert!(
        aggregated
            .visible
            .contains("   Compiling crate_name v0.1.0\n")
    );
    assert_eq!(aggregated.aggregates.len(), 1);
    let collapsed = aggregated.aggregates[0];
    assert!(collapsed.lines >= 50);
    assert!(aggregated.visible.contains(&format!(
        "[distill: {} repeated lines omitted]",
        collapsed.lines
    )));

    // The annotation is carried outside the span partition: the collapsed run
    // is omitted source, and retained plus omitted still cover every byte once.
    assert!(
        aggregated
            .omitted_spans
            .iter()
            .any(|span| { span.start <= collapsed.span.start && collapsed.span.end <= span.end })
    );
    assert_partitions(source.len(), &aggregated);

    // The freed budget buys distinct facts the verbatim plan never reaches.
    let distinct = |projection: &Projection| {
        SUBJECTS
            .iter()
            .filter(|subject| projection.visible.contains(&format!("{subject}: reported")))
            .count()
    };
    assert!(
        distinct(&aggregated) > distinct(&verbatim),
        "aggregation kept {} distinct facts against {}",
        distinct(&aggregated),
        distinct(&verbatim)
    );
    assert!(aggregated.visible_count <= 700);
}

/// US-012: a template that occurs once, or too few times to pay for its
/// annotation, is never replaced by a count.
#[test]
fn aggregation_never_replaces_a_line_that_does_not_repeat() {
    let mut source = String::new();
    for (index, subject) in SUBJECTS.iter().enumerate() {
        source.push_str(&format!("{subject}: appears exactly once\n"));
        source.push_str(&format!("   Compiling crate_name v0.1.{index}\n"));
    }
    let outcome = project(source.as_bytes(), &bytes(400), "build-output/v1").expect("unique");
    // Every collapsed run holds at least the documented minimum of lines, and no
    // isolated repeat was ever replaced.
    assert!(outcome.aggregates.iter().all(|run| run.lines >= 3));
    assert!(!outcome.visible.contains("[distill: 1 repeated"));
    assert!(!outcome.visible.contains("[distill: 2 repeated"));
    assert_partitions(source.len(), &outcome);
}

/// US-012: without redundancy, aggregation changes nothing at all.
#[test]
fn output_without_redundancy_projects_exactly_as_it_did_without_aggregation() {
    let mut source = String::new();
    for subject in SUBJECTS {
        source.push_str(&format!(
            "{subject}: an unrepeatable observation of a distinct subject\n"
        ));
    }
    let budget = bytes(600);
    let aggregated = project(source.as_bytes(), &budget, "build-output/v1").expect("aggregated");
    let verbatim =
        project_without_aggregation(source.as_bytes(), &budget, "build-output/v1").expect("plain");
    assert!(aggregated.aggregates.is_empty());
    assert_eq!(aggregated.visible, verbatim.visible);
    assert_eq!(aggregated.retained_spans, verbatim.retained_spans);
    assert_eq!(aggregated.visible_count, verbatim.visible_count);
}

/// US-013: a shape policy applied to a malformed instance of that shape
/// degrades to line-structured behavior instead of failing.
#[test]
fn a_malformed_instance_of_a_shape_degrades_instead_of_failing() {
    let source = "not a diff at all\n".repeat(200);
    for profile in [
        "unified-diff/v1",
        "api-json/v1",
        "stack-trace/v1",
        "test-output/v1",
        "source-file/v1",
    ] {
        let outcome = project(source.as_bytes(), &bytes(400), profile).expect(profile);
        assert_eq!(outcome.fidelity, Fidelity::Extractive);
        assert!(outcome.visible_count <= 400);
        assert!(outcome.visible.starts_with("not a diff at all\n"));
        assert_partitions(source.len(), &outcome);
    }
}

/// US-014: every retired identifier still resolves, and each one applies the
/// shape policy that its needle table used to approximate. The conformance
/// matrix is the source of the mapping, so the document and the engine cannot
/// drift apart.
#[test]
fn retired_profile_identifiers_resolve_to_their_shape_policy() {
    let matrix: serde_json::Value = serde_json::from_str(include_str!(
        "../../docs/integrations/cli-conformance-v3.json"
    ))
    .expect("CLI conformance matrix");
    let preservation = &matrix["preservation"];
    assert_eq!(preservation["default_profile"], AUTO_PROFILE);

    let retired = preservation["retired_profiles"]
        .as_object()
        .expect("retired profiles");
    assert_eq!(retired.len(), 11);
    for (identifier, applied) in retired {
        let outcome = project(b"one line\n", &bytes(64), identifier).expect(identifier);
        assert_eq!(
            outcome.applied_profile,
            applied.as_str().expect("shape policy"),
            "{identifier}"
        );
    }
    for shape in preservation["shape_profiles"]
        .as_array()
        .expect("shape profiles")
    {
        let identifier = shape.as_str().expect("shape profile");
        let outcome = project(b"one line\n", &bytes(64), identifier).expect(identifier);
        assert_eq!(outcome.applied_profile, identifier);
    }
    // `auto/v1` derives the policy instead of accepting a declared one.
    let detected = project(
        b"diff --git a/x b/x\n@@ -1 +1 @@\n",
        &bytes(64),
        AUTO_PROFILE,
    )
    .expect("detected profile");
    assert_eq!(detected.applied_profile, "unified-diff/v1");
}

fn assert_partitions(source_len: usize, projection: &Projection) {
    let mut coverage = vec![0_u8; source_len];
    for span in projection
        .retained_spans
        .iter()
        .chain(projection.omitted_spans.iter())
    {
        for byte in &mut coverage[span.start as usize..span.end as usize] {
            *byte += 1;
        }
    }
    assert!(coverage.iter().all(|count| *count == 1));
    assert!(projection.retained_spans.len() <= MAX_RECEIPT_SPANS);
    assert!(projection.omitted_spans.len() <= MAX_RECEIPT_SPANS);
}

/// US-012: aggregation stays inside its deadline on the largest real fixture,
/// and it allocates a bounded map rather than one entry per line.
#[test]
fn aggregation_holds_its_deadline_on_the_largest_real_fixture() {
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("evaluation/corpus/real/fixtures");
    let largest = std::fs::read_dir(&directory)
        .expect("real corpus fixtures")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            std::fs::read(path)
                .is_ok_and(|bytes| bytes.iter().filter(|byte| **byte == b'\n').count() >= 100)
        })
        .max_by_key(|path| path.metadata().map(|data| data.len()).unwrap_or(0))
        .expect("largest line-structured fixture");
    let source = std::fs::read(&largest).expect("fixture bytes");
    assert!(source.len() > 64 * 1024, "{}", largest.display());
    let budget = Budget {
        unit: CountUnit::Tokens,
        total_visible_limit: 2_250,
        reserved_envelope: 450,
        token_profile: Some(CL100K_PROFILE.to_owned()),
    };

    let run = |aggregating: bool| {
        if aggregating {
            project(black_box(&source), &budget, "terminal-log/v1").expect("timed")
        } else {
            project_without_aggregation(black_box(&source), &budget, "terminal-log/v1")
                .expect("timed")
        }
    };
    for _ in 0..2 {
        black_box(run(true));
        black_box(run(false));
    }
    // The two conditions are interleaved round by round, so a machine that
    // slows down mid-measurement moves both of them instead of one.
    let mut aggregated_times = Vec::with_capacity(20);
    let mut verbatim_times = Vec::with_capacity(20);
    for _ in 0..20 {
        let started = Instant::now();
        black_box(run(true));
        aggregated_times.push(started.elapsed());
        let started = Instant::now();
        black_box(run(false));
        verbatim_times.push(started.elapsed());
    }

    let aggregated = p95(aggregated_times);
    let verbatim = p95(verbatim_times);
    eprintln!(
        "aggregated P95 {aggregated:?} against {verbatim:?} on {}",
        largest.display()
    );
    assert!(
        aggregated.as_nanos() * 100 <= verbatim.as_nanos() * AGGREGATION_OVERHEAD_PERCENT,
        "aggregation cost {aggregated:?} against {verbatim:?} on {}",
        largest.display()
    );
    assert!(
        aggregated <= Duration::from_millis(AGGREGATED_PROJECTION_DEADLINE_MS),
        "aggregated projection P95 was {aggregated:?} on {}",
        largest.display()
    );
}

/// The grouping pass may cost a documented fraction over the same projection
/// without it, measured on the largest line-structured fixture.
const AGGREGATION_OVERHEAD_PERCENT: u128 = 150;

/// The documented deadline for an aggregated projection of the largest corpus
/// fixture, measured on the unoptimized test profile.
const AGGREGATED_PROJECTION_DEADLINE_MS: u64 = 250;

/// US-013: each shape reduces along its own structure. A trace collapses its
/// library frames and keeps the project ones, a diff drops unchanged context
/// before changed lines, a JSON document keeps its skeleton and elides long
/// values, and a source file is cut only on line boundaries.
#[test]
fn each_shape_reduces_along_its_own_structure() {
    let mut trace = String::from("TypeError: budget must be a number\n");
    trace.push_str("    at loadBudget (/home/dev/work/js/throwing.js:3:15)\n");
    for index in 0..12 {
        trace.push_str(&format!(
            "    at runMicrotasks (node:internal/process/task_queues:9{index}:5)\n"
        ));
    }
    trace.push_str("    at settleBudget (/home/dev/work/js/settle.js:8:3)\n");
    let projected = project(trace.as_bytes(), &bytes(190), "stack-trace/v1").expect("trace");
    assert!(
        projected
            .visible
            .contains("TypeError: budget must be a number")
    );
    assert!(projected.visible.contains("at loadBudget"));
    assert_eq!(projected.aggregates.len(), 1);
    assert!(projected.visible.contains("[distill: 1"));
    assert!(projected.visible.contains("repeated lines omitted]"));
    // The collapsed lines are library frames, not the project ones.
    assert!(
        !projected
            .visible
            .contains("at runMicrotasks (node:internal")
    );

    let mut diff = String::from("diff --git a/src/main.rs b/src/main.rs\n");
    diff.push_str("--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,40 +1,41 @@\n");
    for index in 0..40 {
        diff.push_str(&format!(" unchanged context line {index:02}\n"));
    }
    diff.push_str("+    let budget = payload_limit();\n");
    diff.push_str("-    let budget = 0;\n");
    let projected = project(diff.as_bytes(), &bytes(240), "unified-diff/v1").expect("diff");
    assert!(projected.visible.contains("@@ -1,40 +1,41 @@"));
    assert!(
        projected
            .visible
            .contains("+    let budget = payload_limit();")
    );
    assert!(projected.visible.contains("-    let budget = 0;"));
    assert!(
        !projected.visible.contains("unchanged context line 20"),
        "context was kept before changed lines"
    );
    assert!(projected.aggregates.is_empty(), "a diff never aggregates");

    let mut document = String::from("{\n  \"schema_version\": \"distill.status/v2\",\n");
    document.push_str(&format!("  \"note\": \"{}\",\n", "long value ".repeat(24)));
    document.push_str("  \"records\": [\n");
    for index in 0..40 {
        document.push_str(&format!(
            "    {{ \"id\": \"record-{index:02}\", \"status\": \"ok\" }},\n"
        ));
    }
    document.push_str("  ],\n  \"failure_code\": \"store_corrupt\"\n}\n");
    let projected = project(document.as_bytes(), &bytes(200), "api-json/v1").expect("json");
    assert!(projected.visible.contains("\"schema_version\""));
    assert!(projected.visible.contains("\"failure_code\""));
    assert!(
        !projected.visible.contains("long value long value"),
        "a long value survived its skeleton"
    );

    let mut file = String::from("use crate::artifact::Receipt;\n\n");
    for index in 0..40 {
        file.push_str(&format!(
            "pub fn helper_{index:02}(value: u64) -> u64 {{\n    value.saturating_add({index})\n}}\n\n"
        ));
    }
    let projected = project(file.as_bytes(), &bytes(400), "source-file/v1").expect("source");
    assert!(projected.aggregates.is_empty(), "source never aggregates");
    for span in &projected.retained_spans {
        if span.start > 0 {
            assert_eq!(file.as_bytes()[span.start as usize - 1], b'\n');
        }
        if (span.end as usize) < file.len() {
            assert_eq!(file.as_bytes()[span.end as usize - 1], b'\n');
        }
    }
    assert_partitions(file.len(), &projected);
}
