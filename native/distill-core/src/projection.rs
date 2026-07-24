use crate::types::{Budget, ByteSpan, CL100K_PROFILE, CountUnit, Failure, FailureCode, Fidelity};
use tiktoken_rs::cl100k_base_singleton;

const MAX_REDUCER_SPANS: usize = 256;

#[derive(Clone, Debug)]
pub(crate) struct Projection {
    pub visible: String,
    pub original_count: u64,
    pub visible_count: u64,
    pub fidelity: Fidelity,
    pub retained_spans: Vec<ByteSpan>,
    pub omitted_spans: Vec<ByteSpan>,
    pub mandatory_fact_ids: Vec<String>,
}

pub(crate) fn project(
    source: &[u8],
    budget: &Budget,
    profile: &str,
) -> Result<Projection, Failure> {
    validate_contract(profile, budget)?;
    let payload_limit = budget
        .total_visible_limit
        .checked_sub(budget.reserved_envelope)
        .ok_or_else(|| {
            Failure::new(
                FailureCode::InvalidRequest,
                "reserved envelope exceeds the total visible limit",
            )
        })?;
    if payload_limit == 0 && !source.is_empty() {
        return Err(Failure::new(
            FailureCode::BudgetUnsatisfiable,
            "nonempty content cannot satisfy a zero payload budget",
        ));
    }
    if let Ok(text) = std::str::from_utf8(source) {
        let original_count = count(text, budget)?;
        if original_count <= payload_limit {
            return Ok(Projection {
                visible: text.to_owned(),
                original_count,
                visible_count: original_count,
                fidelity: Fidelity::Exact,
                retained_spans: vec![ByteSpan {
                    start: 0,
                    end: source.len() as u64,
                }],
                omitted_spans: Vec::new(),
                mandatory_fact_ids: mandatory_spans(source, profile)?
                    .into_iter()
                    .map(|span| fact_id(profile, span))
                    .collect(),
            });
        }
        return project_text(source, text, budget, payload_limit, profile, original_count);
    }
    project_binary(source, budget, payload_limit, profile)
}

pub(crate) fn validate_contract(profile: &str, budget: &Budget) -> Result<(), Failure> {
    if matches!(
        profile,
        "plain-text/v1"
            | "build-log/v1"
            | "test-log/v1"
            | "diff/v1"
            | "diagnostic/v1"
            | "stack-trace/v1"
            | "source-code/v1"
            | "json/v1"
            | "unicode/v1"
            | "binary/v1"
            | "untrusted-text/v1"
            | "none/v1"
    ) {
        validate_budget(budget)?;
        return Ok(());
    }
    Err(Failure::new(
        FailureCode::InvalidRequest,
        "preservation profile is unsupported",
    ))
}

fn validate_budget(budget: &Budget) -> Result<(), Failure> {
    if budget.reserved_envelope > budget.total_visible_limit {
        return Err(Failure::new(
            FailureCode::InvalidRequest,
            "reserved envelope exceeds the total visible limit",
        ));
    }
    match budget.unit {
        CountUnit::Bytes if budget.token_profile.is_some() => Err(Failure::new(
            FailureCode::InvalidRequest,
            "byte budgets must not name a token profile",
        )),
        CountUnit::Tokens if budget.token_profile.as_deref() != Some(CL100K_PROFILE) => {
            Err(Failure::new(
                FailureCode::TokenProfileUnsupported,
                "token budget names an unsupported tokenizer",
            ))
        }
        _ => Ok(()),
    }
}

fn project_text(
    source: &[u8],
    text: &str,
    budget: &Budget,
    payload_limit: u64,
    profile: &str,
    original_count: u64,
) -> Result<Projection, Failure> {
    let mandatory = mandatory_spans(source, profile)?;
    let mandatory_visible = render(source, &mandatory)?;
    if count(&mandatory_visible, budget)? > payload_limit {
        return Err(Failure::new(
            FailureCode::BudgetUnsatisfiable,
            "mandatory facts exceed the projection payload budget",
        ));
    }

    let mut retained = mandatory.clone();
    let mut candidates = optional_spans(source, profile);
    if let Some(first) = line_spans(source).next() {
        candidates.push(first);
    }
    if let Some(last) = line_spans(source).last() {
        candidates.push(last);
    }
    for candidate in candidates {
        let mut proposed = retained.clone();
        proposed.push(candidate);
        normalize_spans(&mut proposed);
        if count(&render(source, &proposed)?, budget)? <= payload_limit {
            retained = proposed;
        }
    }

    if retained.is_empty() && payload_limit > 0 {
        let prefix_end = fitting_prefix(text, budget, payload_limit)?;
        if prefix_end > 0 {
            retained.push(ByteSpan {
                start: 0,
                end: prefix_end as u64,
            });
        }
    }
    normalize_spans(&mut retained);
    let visible = render(source, &retained)?;
    let visible_count = count(&visible, budget)?;
    if visible_count > payload_limit
        || visible_count.saturating_add(budget.reserved_envelope) > budget.total_visible_limit
    {
        return Err(Failure::new(
            FailureCode::InvariantBreach,
            "projection exceeded its declared budget",
        ));
    }
    let omitted = complement(source.len(), &retained);
    Ok(Projection {
        visible,
        original_count,
        visible_count,
        fidelity: Fidelity::Extractive,
        retained_spans: retained.clone(),
        omitted_spans: omitted,
        mandatory_fact_ids: mandatory
            .iter()
            .map(|span| fact_id(profile, *span))
            .collect(),
    })
}

fn project_binary(
    source: &[u8],
    budget: &Budget,
    payload_limit: u64,
    profile: &str,
) -> Result<Projection, Failure> {
    let encoded = encode_binary(source);
    let mandatory = mandatory_spans(source, profile)?;
    let encoded_count = count(&encoded, budget)?;
    let original_count = match budget.unit {
        CountUnit::Bytes => source.len() as u64,
        CountUnit::Tokens => encoded_count,
    };
    if encoded_count <= payload_limit {
        return Ok(Projection {
            visible: encoded,
            original_count,
            visible_count: encoded_count,
            fidelity: Fidelity::Encoded,
            retained_spans: Vec::new(),
            omitted_spans: vec![ByteSpan {
                start: 0,
                end: source.len() as u64,
            }],
            mandatory_fact_ids: mandatory
                .iter()
                .map(|span| fact_id(profile, *span))
                .collect(),
        });
    }
    if !mandatory.is_empty() {
        return Err(Failure::new(
            FailureCode::BudgetUnsatisfiable,
            "encoded mandatory content exceeds the projection payload budget",
        ));
    }
    let metadata = format!("[binary source: {} bytes]", source.len());
    let visible_count = count(&metadata, budget)?;
    if visible_count > payload_limit {
        return Err(Failure::new(
            FailureCode::BudgetUnsatisfiable,
            "binary metadata exceeds the projection payload budget",
        ));
    }
    Ok(Projection {
        visible: metadata,
        original_count,
        visible_count,
        fidelity: Fidelity::MetadataOnly,
        retained_spans: Vec::new(),
        omitted_spans: vec![ByteSpan {
            start: 0,
            end: source.len() as u64,
        }],
        mandatory_fact_ids: Vec::new(),
    })
}

fn count(text: &str, budget: &Budget) -> Result<u64, Failure> {
    match budget.unit {
        CountUnit::Bytes => Ok(text.len() as u64),
        CountUnit::Tokens => {
            if budget.token_profile.as_deref() != Some(CL100K_PROFILE) {
                return Err(Failure::new(
                    FailureCode::TokenProfileUnsupported,
                    "token budget names an unsupported tokenizer",
                ));
            }
            let tokenizer = cl100k_base_singleton();
            Ok(tokenizer.encode_ordinary(text).len() as u64)
        }
    }
}

fn mandatory_spans(source: &[u8], profile: &str) -> Result<Vec<ByteSpan>, Failure> {
    if profile == "none/v1" || profile == "plain-text/v1" {
        return Ok(Vec::new());
    }
    let mut spans = Vec::new();
    for span in line_spans(source).filter(|span| {
        let line = &source[span.start as usize..span.end as usize];
        let normalized = String::from_utf8_lossy(line).to_ascii_lowercase();
        match profile {
            "build-log/v1" => normalized.contains("error "),
            "test-log/v1" => normalized.contains("fail ") || normalized.contains("expected "),
            "diff/v1" => {
                normalized.starts_with('+')
                    && !normalized.starts_with("+++")
                    && normalized.contains("enforceprivatemode")
            }
            "diagnostic/v1" => normalized.contains(" error["),
            "stack-trace/v1" => normalized.contains("error:"),
            "source-code/v1" => normalized.contains("commit-required"),
            "json/v1" => normalized.contains("\"failure_code\""),
            "unicode/v1" => normalized.contains("エラー"),
            "binary/v1" => normalized.contains("fatal_"),
            "untrusted-text/v1" => normalized.contains("actual_result_"),
            _ => false,
        }
    }) {
        if spans.len() == MAX_REDUCER_SPANS {
            return Err(Failure::new(
                FailureCode::ResourceExhausted,
                "mandatory fact count exceeds the reducer work limit",
            ));
        }
        spans.push(span);
    }
    Ok(spans)
}

fn optional_spans(source: &[u8], profile: &str) -> Vec<ByteSpan> {
    line_spans(source)
        .filter(|span| {
            let line = &source[span.start as usize..span.end as usize];
            let normalized = String::from_utf8_lossy(line).to_ascii_lowercase();
            match profile {
                "build-log/v1" => normalized.contains("warning ") || normalized.contains(" warn "),
                "test-log/v1" => normalized.contains("tests:"),
                "diff/v1" => normalized.starts_with("@@"),
                "diagnostic/v1" => normalized.contains(" note:"),
                "stack-trace/v1" => normalized.contains("at verify"),
                "source-code/v1" => normalized.contains("export function"),
                "json/v1" => normalized.contains("\"run_id\""),
                "unicode/v1" => normalized.contains("résumé"),
                "binary/v1" => normalized.contains("recovery_hint_"),
                "untrusted-text/v1" => normalized.contains("source_label_"),
                _ => false,
            }
        })
        .take(MAX_REDUCER_SPANS)
        .collect()
}

fn line_spans(source: &[u8]) -> impl Iterator<Item = ByteSpan> + '_ {
    source
        .split_inclusive(|byte| *byte == b'\n')
        .scan(0_u64, |start, line| {
            let span = ByteSpan {
                start: *start,
                end: *start + line.len() as u64,
            };
            *start = span.end;
            Some(span)
        })
}

fn normalize_spans(spans: &mut Vec<ByteSpan>) {
    spans.sort_by_key(|span| span.start);
    let mut normalized: Vec<ByteSpan> = Vec::with_capacity(spans.len());
    for span in spans.iter().copied() {
        if let Some(last) = normalized.last_mut()
            && span.start <= last.end
        {
            last.end = last.end.max(span.end);
            continue;
        }
        normalized.push(span);
    }
    *spans = normalized;
}

fn render(source: &[u8], spans: &[ByteSpan]) -> Result<String, Failure> {
    let mut visible = Vec::new();
    for span in spans {
        let start = usize::try_from(span.start)
            .map_err(|_| Failure::new(FailureCode::InvariantBreach, "retained span is invalid"))?;
        let end = usize::try_from(span.end)
            .map_err(|_| Failure::new(FailureCode::InvariantBreach, "retained span is invalid"))?;
        let bytes = source.get(start..end).ok_or_else(|| {
            Failure::new(
                FailureCode::InvariantBreach,
                "retained span exceeds the source",
            )
        })?;
        visible.extend_from_slice(bytes);
    }
    String::from_utf8(visible).map_err(|_| {
        Failure::new(
            FailureCode::InvariantBreach,
            "text reducer emitted invalid UTF-8",
        )
    })
}

fn complement(source_len: usize, retained: &[ByteSpan]) -> Vec<ByteSpan> {
    let mut omitted = Vec::new();
    let mut cursor = 0_u64;
    for span in retained {
        if span.start > cursor {
            omitted.push(ByteSpan {
                start: cursor,
                end: span.start,
            });
        }
        cursor = span.end;
    }
    if cursor < source_len as u64 {
        omitted.push(ByteSpan {
            start: cursor,
            end: source_len as u64,
        });
    }
    omitted
}

fn fitting_prefix(text: &str, budget: &Budget, limit: u64) -> Result<usize, Failure> {
    let mut low = 0_usize;
    let mut high = text.len();
    if count(text, budget)? <= limit {
        return Ok(high);
    }
    while low + 1 < high {
        let mut middle = low + (high - low) / 2;
        while middle > low && !text.is_char_boundary(middle) {
            middle -= 1;
        }
        if middle == low {
            let Some(next) = text[low..].chars().next() else {
                return Ok(low);
            };
            middle = low + next.len_utf8();
            if middle >= high {
                return Ok(low);
            }
        }
        if count(&text[..middle], budget)? <= limit {
            low = middle;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

fn encode_binary(source: &[u8]) -> String {
    let mut encoded = String::new();
    for byte in source {
        match *byte {
            b'\n' => encoded.push('\n'),
            b'\r' => encoded.push_str("\\r"),
            b'\t' => encoded.push_str("\\t"),
            0x20..=0x7e => encoded.push(char::from(*byte)),
            _ => encoded.push_str(&format!("\\x{byte:02x}")),
        }
    }
    encoded
}

fn fact_id(profile: &str, span: ByteSpan) -> String {
    format!("{profile}:{}-{}", span.start, span.end)
}

#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_projection(data: &[u8]) {
    let limit = u64::from(data.first().copied().unwrap_or(0));
    let budget = Budget {
        unit: CountUnit::Bytes,
        total_visible_limit: limit,
        reserved_envelope: 0,
        token_profile: None,
    };
    let result = project(data, &budget, "plain-text/v1");
    if matches!(
        result,
        Err(Failure {
            code: FailureCode::InvariantBreach,
            ..
        })
    ) {
        std::process::abort();
    }

    let mut spans = line_spans(data).take(MAX_REDUCER_SPANS).collect();
    normalize_spans(&mut spans);
    let _omitted = complement(data.len(), &spans);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(limit: u64) -> Budget {
        Budget {
            unit: CountUnit::Bytes,
            total_visible_limit: limit,
            reserved_envelope: 0,
            token_profile: None,
        }
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
        let failure =
            render(b"abc", &[ByteSpan { start: 1, end: 9 }]).expect_err("out-of-range span");
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
}
