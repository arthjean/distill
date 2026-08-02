use crate::types::{Budget, ByteSpan, CL100K_PROFILE, CountUnit, Failure, FailureCode, Fidelity};
use tiktoken_rs::cl100k_base_singleton;

const MAX_REDUCER_SPANS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineRule {
    Never,
    Contains(&'static str),
    ContainsAny(&'static [&'static str]),
    StartsWith(&'static str),
    AddedPrivateMode,
}

impl LineRule {
    fn matches(self, line: &str) -> bool {
        match self {
            Self::Never => false,
            Self::Contains(needle) => line.contains(needle),
            Self::ContainsAny(needles) => needles.iter().any(|needle| line.contains(needle)),
            Self::StartsWith(prefix) => line.starts_with(prefix),
            Self::AddedPrivateMode => {
                line.starts_with('+')
                    && !line.starts_with("+++")
                    && line.contains("enforceprivatemode")
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Policy {
    id: &'static str,
    mandatory: LineRule,
    optional: LineRule,
}

const POLICIES: &[Policy] = &[
    Policy {
        id: "plain-text/v1",
        mandatory: LineRule::Never,
        optional: LineRule::Never,
    },
    Policy {
        id: "build-log/v1",
        mandatory: LineRule::Contains("error "),
        optional: LineRule::ContainsAny(&["warning ", " warn "]),
    },
    Policy {
        id: "test-log/v1",
        mandatory: LineRule::ContainsAny(&["fail ", "expected "]),
        optional: LineRule::Contains("tests:"),
    },
    Policy {
        id: "diff/v1",
        mandatory: LineRule::AddedPrivateMode,
        optional: LineRule::StartsWith("@@"),
    },
    Policy {
        id: "diagnostic/v1",
        mandatory: LineRule::Contains(" error["),
        optional: LineRule::Contains(" note:"),
    },
    Policy {
        id: "stack-trace/v1",
        mandatory: LineRule::Contains("error:"),
        optional: LineRule::Contains("at verify"),
    },
    Policy {
        id: "source-code/v1",
        mandatory: LineRule::Contains("commit-required"),
        optional: LineRule::Contains("export function"),
    },
    Policy {
        id: "json/v1",
        mandatory: LineRule::Contains("\"failure_code\""),
        optional: LineRule::Contains("\"run_id\""),
    },
    Policy {
        id: "unicode/v1",
        mandatory: LineRule::Contains("エラー"),
        optional: LineRule::Contains("résumé"),
    },
    Policy {
        id: "binary/v1",
        mandatory: LineRule::Contains("fatal_"),
        optional: LineRule::Contains("recovery_hint_"),
    },
    Policy {
        id: "untrusted-text/v1",
        mandatory: LineRule::Contains("actual_result_"),
        optional: LineRule::Contains("source_label_"),
    },
    Policy {
        id: "none/v1",
        mandatory: LineRule::Never,
        optional: LineRule::Never,
    },
];

#[derive(Debug)]
struct Analysis {
    mandatory: Vec<ByteSpan>,
    candidates: Vec<ByteSpan>,
}

#[derive(Clone, Debug)]
struct SpanPlan {
    spans: Vec<ByteSpan>,
    byte_count: u64,
}

impl SpanPlan {
    fn new(mut spans: Vec<ByteSpan>) -> Self {
        normalize_spans(&mut spans);
        let byte_count = span_byte_count(&spans);
        Self { spans, byte_count }
    }

    fn with_candidate(&self, candidate: ByteSpan) -> Self {
        let mut spans = Vec::with_capacity(self.spans.len() + 1);
        let mut merged = candidate;
        let mut inserted = false;
        for span in self.spans.iter().copied() {
            if span.end < merged.start {
                spans.push(span);
            } else if merged.end < span.start {
                if !inserted {
                    spans.push(merged);
                    inserted = true;
                }
                spans.push(span);
            } else {
                merged.start = merged.start.min(span.start);
                merged.end = merged.end.max(span.end);
            }
        }
        if !inserted {
            spans.push(merged);
        }
        let byte_count = span_byte_count(&spans);
        Self { spans, byte_count }
    }
}

#[cfg(test)]
#[derive(Default)]
struct PlanningMetrics {
    full_render_count_evaluations: usize,
}

#[cfg(test)]
thread_local! {
    static FULL_RENDER_COUNT_EVALUATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_full_render_count_evaluation() {
    FULL_RENDER_COUNT_EVALUATIONS.with(|count| count.set(count.get() + 1));
}

#[derive(Clone, Copy, Debug)]
enum Counter {
    Bytes,
    Cl100k,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProjectionSpec {
    policy: &'static Policy,
    counter: Counter,
    total_visible_limit: u64,
    reserved_envelope: u64,
}

impl ProjectionSpec {
    pub(crate) fn new(profile: &str, budget: &Budget) -> Result<Self, Failure> {
        let policy = POLICIES
            .iter()
            .find(|candidate| candidate.id == profile)
            .ok_or_else(|| {
                Failure::new(
                    FailureCode::InvalidRequest,
                    "preservation profile is unsupported",
                )
            })?;
        if budget.reserved_envelope > budget.total_visible_limit {
            return Err(Failure::new(
                FailureCode::InvalidRequest,
                "reserved envelope exceeds the total visible limit",
            ));
        }
        let counter = match budget.unit {
            CountUnit::Bytes if budget.token_profile.is_none() => Counter::Bytes,
            CountUnit::Bytes => {
                return Err(Failure::new(
                    FailureCode::InvalidRequest,
                    "byte budgets must not name a token profile",
                ));
            }
            CountUnit::Tokens if budget.token_profile.as_deref() == Some(CL100K_PROFILE) => {
                Counter::Cl100k
            }
            CountUnit::Tokens => {
                return Err(Failure::new(
                    FailureCode::TokenProfileUnsupported,
                    "token budget names an unsupported tokenizer",
                ));
            }
        };
        Ok(Self {
            policy,
            counter,
            total_visible_limit: budget.total_visible_limit,
            reserved_envelope: budget.reserved_envelope,
        })
    }

    pub(crate) fn unit(self) -> CountUnit {
        match self.counter {
            Counter::Bytes => CountUnit::Bytes,
            Counter::Cl100k => CountUnit::Tokens,
        }
    }

    pub(crate) fn token_profile(self) -> Option<String> {
        matches!(self.counter, Counter::Cl100k).then(|| CL100K_PROFILE.to_owned())
    }

    pub(crate) fn profile(self) -> &'static str {
        self.policy.id
    }

    fn count(self, text: &str) -> u64 {
        match self.counter {
            Counter::Bytes => text.len() as u64,
            Counter::Cl100k => cl100k_base_singleton().encode_ordinary(text).len() as u64,
        }
    }

    fn payload_limit(self) -> u64 {
        self.total_visible_limit - self.reserved_envelope
    }

    pub(crate) fn accounts_for(self, visible_count: u64) -> bool {
        visible_count.saturating_add(self.reserved_envelope) <= self.total_visible_limit
    }
}

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

pub(crate) fn project_validated(
    source: &[u8],
    spec: ProjectionSpec,
) -> Result<Projection, Failure> {
    let payload_limit = spec.payload_limit();
    if payload_limit == 0 && !source.is_empty() {
        return Err(Failure::new(
            FailureCode::BudgetUnsatisfiable,
            "nonempty content cannot satisfy a zero payload budget",
        ));
    }
    if let Ok(text) = std::str::from_utf8(source) {
        let original_count = spec.count(text);
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
                mandatory_fact_ids: mandatory_spans(source, spec.policy)?
                    .into_iter()
                    .map(|span| fact_id(spec.policy.id, span))
                    .collect(),
            });
        }
        return project_text(source, text, spec, payload_limit, original_count);
    }
    project_binary(source, spec, payload_limit)
}

fn project_text(
    source: &[u8],
    text: &str,
    spec: ProjectionSpec,
    payload_limit: u64,
    original_count: u64,
) -> Result<Projection, Failure> {
    let analysis = analyze(source, spec.policy)?;
    let mandatory = analysis.mandatory;
    let mut retained = SpanPlan::new(mandatory.clone());
    if !plan_fits(source, &retained, spec, payload_limit)? {
        return Err(Failure::new(
            FailureCode::BudgetUnsatisfiable,
            "mandatory facts exceed the projection payload budget",
        ));
    }

    for candidate in analysis.candidates {
        let proposed = retained.with_candidate(candidate);
        if plan_fits(source, &proposed, spec, payload_limit)? {
            retained = proposed;
        }
    }

    if retained.spans.is_empty() && payload_limit > 0 {
        let prefix_end = fitting_prefix_validated(text, spec, payload_limit);
        if prefix_end > 0 {
            retained = SpanPlan::new(vec![ByteSpan {
                start: 0,
                end: prefix_end as u64,
            }]);
        }
    }
    let visible = render(source, &retained.spans)?;
    #[cfg(test)]
    record_full_render_count_evaluation();
    let visible_count = spec.count(&visible);
    if visible_count > payload_limit
        || visible_count.saturating_add(spec.reserved_envelope) > spec.total_visible_limit
    {
        return Err(Failure::new(
            FailureCode::InvariantBreach,
            "projection exceeded its declared budget",
        ));
    }
    Ok(Projection {
        visible,
        original_count,
        visible_count,
        fidelity: Fidelity::Extractive,
        retained_spans: retained.spans.clone(),
        omitted_spans: complement(source.len(), &retained.spans),
        mandatory_fact_ids: mandatory
            .iter()
            .map(|span| fact_id(spec.policy.id, *span))
            .collect(),
    })
}

fn project_binary(
    source: &[u8],
    spec: ProjectionSpec,
    payload_limit: u64,
) -> Result<Projection, Failure> {
    let encoded = encode_binary(source);
    let mandatory = mandatory_spans(source, spec.policy)?;
    let encoded_count = spec.count(&encoded);
    let original_count = match spec.unit() {
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
                .map(|span| fact_id(spec.policy.id, *span))
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
    let visible_count = spec.count(&metadata);
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

fn mandatory_spans(source: &[u8], policy: &Policy) -> Result<Vec<ByteSpan>, Failure> {
    if policy.mandatory == LineRule::Never {
        return Ok(Vec::new());
    }
    let mut spans = Vec::new();
    for span in line_spans(source) {
        let line = &source[span.start as usize..span.end as usize];
        let normalized = String::from_utf8_lossy(line).to_ascii_lowercase();
        if policy.mandatory.matches(&normalized) {
            if spans.len() == MAX_REDUCER_SPANS {
                return Err(Failure::new(
                    FailureCode::ResourceExhausted,
                    "mandatory fact count exceeds the reducer work limit",
                ));
            }
            spans.push(span);
        }
    }
    Ok(spans)
}

fn analyze(source: &[u8], policy: &Policy) -> Result<Analysis, Failure> {
    let mut mandatory = Vec::new();
    let mut candidates = Vec::new();
    let mut first = None;
    let mut last = None;
    for span in line_spans(source) {
        first.get_or_insert(span);
        last = Some(span);
        let line = &source[span.start as usize..span.end as usize];
        let normalized = String::from_utf8_lossy(line).to_ascii_lowercase();
        if policy.mandatory.matches(&normalized) {
            if mandatory.len() == MAX_REDUCER_SPANS {
                return Err(Failure::new(
                    FailureCode::ResourceExhausted,
                    "mandatory fact count exceeds the reducer work limit",
                ));
            }
            mandatory.push(span);
        }
        if candidates.len() < MAX_REDUCER_SPANS && policy.optional.matches(&normalized) {
            candidates.push(span);
        }
    }
    if let Some(first) = first {
        candidates.push(first);
    }
    if let Some(last) = last {
        candidates.push(last);
    }
    Ok(Analysis {
        mandatory,
        candidates,
    })
}

fn plan_fits(
    source: &[u8],
    plan: &SpanPlan,
    spec: ProjectionSpec,
    payload_limit: u64,
) -> Result<bool, Failure> {
    match spec.unit() {
        CountUnit::Bytes => Ok(plan.byte_count <= payload_limit),
        CountUnit::Tokens => {
            // Every ordinary cl100k token consumes at least one UTF-8 byte, so
            // byte length is a safe upper bound when it already fits.
            if plan.byte_count <= payload_limit {
                return Ok(true);
            }
            #[cfg(test)]
            record_full_render_count_evaluation();
            Ok(spec.count(&render(source, &plan.spans)?) <= payload_limit)
        }
    }
}

fn span_byte_count(spans: &[ByteSpan]) -> u64 {
    spans
        .iter()
        .map(|span| span.end.saturating_sub(span.start))
        .sum()
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

fn fitting_prefix_validated(text: &str, spec: ProjectionSpec, limit: u64) -> usize {
    let mut low = 0_usize;
    let mut high = text.len();
    if spec.count(text) <= limit {
        return high;
    }
    while low + 1 < high {
        let mut middle = low + (high - low) / 2;
        while middle > low && !text.is_char_boundary(middle) {
            middle -= 1;
        }
        if middle == low {
            let Some(next) = text[low..].chars().next() else {
                return low;
            };
            middle = low + next.len_utf8();
            if middle >= high {
                return low;
            }
        }
        if spec.count(&text[..middle]) <= limit {
            low = middle;
        } else {
            high = middle;
        }
    }
    low
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
    let result = ProjectionSpec::new("plain-text/v1", &budget)
        .and_then(|spec| project_validated(data, spec));
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
fn validated_policy(profile: &str, budget: &Budget) -> Result<&'static Policy, Failure> {
    ProjectionSpec::new(profile, budget).map(|spec| spec.policy)
}

#[cfg(test)]
fn count(text: &str, budget: &Budget) -> Result<u64, Failure> {
    ProjectionSpec::new("plain-text/v1", budget).map(|spec| spec.count(text))
}

#[cfg(test)]
fn fitting_prefix(text: &str, budget: &Budget, limit: u64) -> Result<usize, Failure> {
    ProjectionSpec::new("plain-text/v1", budget)
        .map(|spec| fitting_prefix_validated(text, spec, limit))
}

#[cfg(test)]
fn project(source: &[u8], budget: &Budget, profile: &str) -> Result<Projection, Failure> {
    ProjectionSpec::new(profile, budget).and_then(|spec| project_validated(source, spec))
}

#[cfg(test)]
fn project_with_metrics(
    source: &[u8],
    budget: &Budget,
    profile: &str,
    metrics: &mut PlanningMetrics,
) -> Result<Projection, Failure> {
    FULL_RENDER_COUNT_EVALUATIONS.with(|count| count.set(0));
    let result = project(source, budget, profile);
    metrics.full_render_count_evaluations =
        FULL_RENDER_COUNT_EVALUATIONS.with(std::cell::Cell::get);
    result
}

#[cfg(test)]
mod tests {
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
        assert!(
            count(&mandatory_visible, budget).expect("baseline mandatory count") <= payload_limit
        );

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
        let planned =
            project_with_metrics(source.as_bytes(), &budget, "build-log/v1", &mut metrics)
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
}
