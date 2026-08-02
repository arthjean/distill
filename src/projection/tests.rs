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

fn baseline_project(source: &[u8], budget: &Budget, profile: &str) -> (Projection, usize) {
    let policy = validated_policy(profile, budget).expect("baseline policy");
    let text = std::str::from_utf8(source).expect("baseline text");
    let original_count = count(text, budget).expect("baseline original count");
    let payload_limit = budget.total_visible_limit - budget.reserved_envelope;
    assert!(original_count > payload_limit);

    let analysis = analyze(source, policy).expect("baseline analysis");
    let mandatory = analysis.mandatory;
    let mut evaluations = 1;
    let mandatory_visible = render(source, &mandatory).expect("baseline mandatory render");
    assert!(count(&mandatory_visible, budget).expect("baseline mandatory count") <= payload_limit);

    let mut retained = mandatory.clone();
    for candidate in analysis.candidates {
        let mut proposed = retained.clone();
        proposed.push(candidate);
        normalize_spans(&mut proposed);
        evaluations += 1;
        if count(
            &render(source, &proposed).expect("baseline proposal render"),
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
    let visible = render(source, &retained).expect("baseline final render");
    let visible_count = count(&visible, budget).expect("baseline final count");
    (
        Projection {
            visible,
            original_count,
            visible_count,
            fidelity: Fidelity::Extractive,
            retained_spans: retained.clone(),
            omitted_spans: complement(source.len(), &retained),
            mandatory_fact_ids: mandatory
                .iter()
                .map(|span| fact_id(policy.id, *span))
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

#[test]
fn every_v1_reducer_preserves_mandatory_and_optional_facts() {
    let cases = [
        ("build-log/v1", "ERROR E1: critical", "warning W1: useful"),
        (
            "test-log/v1",
            "FAIL session boundary",
            "Tests: 4 passed, 1 failed",
        ),
        (
            "diff/v1",
            "+ enforcePrivateMode(root, 0o700);",
            "@@ -1,2 +1,3 @@",
        ),
        (
            "diagnostic/v1",
            "src/main.rs:1:2 error[E1]: moved",
            "src/lib.rs:2:3 note: moved here",
        ),
        (
            "stack-trace/v1",
            "ArtifactIntegrityError: corrupt",
            "at verifyArtifact (src/store.ts:1:2)",
        ),
        (
            "source-code/v1",
            "if (!committed) return \"commit-required\";",
            "export function verify() {",
        ),
        (
            "json/v1",
            "\"failure_code\": \"STORE_CORRUPT\"",
            "\"run_id\": \"run-1\"",
        ),
        (
            "unicode/v1",
            "エラー: 保存に失敗",
            "Résumé: Αθήνα, مرحبا, 🧪",
        ),
        (
            "untrusted-text/v1",
            "ACTUAL_RESULT_1: checksum failed",
            "SOURCE_LABEL_1: untrusted",
        ),
    ];
    for (profile, mandatory, optional) in cases {
        let source = format!(
            "header\n{}\n{}\n{}\ntail\n",
            "noise\n".repeat(80),
            optional,
            mandatory
        );
        let outcome = project(source.as_bytes(), &bytes(180), profile).expect(profile);
        assert!(
            outcome.visible.contains(mandatory),
            "{profile} omitted mandatory content"
        );
        assert!(
            outcome.visible.contains(optional),
            "{profile} omitted optional content"
        );
        assert!(outcome.visible_count <= 180);
        assert_eq!(outcome.fidelity, Fidelity::Extractive);
    }
}

#[test]
fn impossible_mandatory_content_fails_closed() {
    let source = b"noise\nERROR E123: mandatory compiler diagnostic\nnoise\n";
    let failure = project(source, &bytes(4), "build-log/v1").expect_err("unsatisfiable");
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
    let first = project(source, &bytes(50), "build-log/v1").expect("projection");
    for _ in 0..100 {
        let repeated = project(source, &bytes(50), "build-log/v1").expect("repeat");
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

    let mandatory = "ERROR E1\n".repeat(MAX_REDUCER_SPANS + 1);
    assert_eq!(
        project(mandatory.as_bytes(), &bytes(64), "build-log/v1")
            .expect_err("mandatory span cap")
            .code,
        FailureCode::ResourceExhausted
    );
}

#[test]
fn helpers_reject_invalid_spans_and_normalize_overlap() {
    let failure = render(b"abc", &[ByteSpan { start: 1, end: 9 }]).expect_err("out-of-range span");
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
fn policy_heavy_planning_reuses_analysis_without_latency_regression() {
    let mut source = String::from("ERROR E1: mandatory fact\n");
    for index in 0..MAX_REDUCER_SPANS {
        source.push_str(&format!("warning W{index:03}: optional fact\n"));
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
        baseline_project(source.as_bytes(), &budget, "build-log/v1");
    let mut metrics = PlanningMetrics::default();
    let planned = project_with_metrics(source.as_bytes(), &budget, "build-log/v1", &mut metrics)
        .expect("planned projection");
    assert_eq!(planned.visible, baseline.visible);
    assert_eq!(planned.visible_count, baseline.visible_count);
    assert_eq!(planned.retained_spans, baseline.retained_spans);
    assert_eq!(planned.omitted_spans, baseline.omitted_spans);
    assert_eq!(planned.mandatory_fact_ids, baseline.mandatory_fact_ids);
    assert!(planned.visible.contains("ERROR E1: mandatory fact"));
    assert!(planned.visible.contains("warning W000: optional fact"));
    assert!(
        metrics.full_render_count_evaluations * 2 <= baseline_evaluations,
        "{} optimized evaluations did not halve the {baseline_evaluations} baseline",
        metrics.full_render_count_evaluations
    );

    for _ in 0..2 {
        black_box(baseline_project(
            black_box(source.as_bytes()),
            black_box(&budget),
            "build-log/v1",
        ));
        let mut warmup_metrics = PlanningMetrics::default();
        black_box(
            project_with_metrics(
                black_box(source.as_bytes()),
                black_box(&budget),
                "build-log/v1",
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
                "build-log/v1",
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
                    "build-log/v1",
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
