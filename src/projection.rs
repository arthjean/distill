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
#[path = "projection/tests.rs"]
mod tests;
