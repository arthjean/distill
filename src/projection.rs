use crate::{
    contract::MAX_RECEIPT_SPANS,
    types::{Budget, ByteSpan, CL100K_PROFILE, CountUnit, Failure, FailureCode, Fidelity},
};
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

#[derive(Clone, Copy, Debug)]
struct PlanSpan {
    span: ByteSpan,
    /// Exact count of the source slice the span covers, under the plan counter.
    count: u64,
}

/// A retained selection together with the exact count of the payload it would
/// render. Every span boundary sits on an additive offset, so the payload count
/// is the sum of the span counts and stays exact as candidates are accepted.
#[derive(Clone, Debug, Default)]
struct SpanPlan {
    spans: Vec<PlanSpan>,
    byte_count: u64,
    visible_count: u64,
}

impl SpanPlan {
    fn new(spec: ProjectionSpec, text: &str, spans: &[ByteSpan]) -> Self {
        spans.iter().fold(Self::default(), |plan, span| {
            plan.with_candidate(spec, text, *span)
        })
    }

    fn prefix(span: ByteSpan, count: u64) -> Self {
        Self {
            spans: vec![PlanSpan { span, count }],
            byte_count: span.end.saturating_sub(span.start),
            visible_count: count,
        }
    }

    fn ranges(&self) -> Vec<ByteSpan> {
        self.spans.iter().map(|entry| entry.span).collect()
    }

    /// Merges one candidate into the plan, counting only the source the plan
    /// did not already cover. Overlapping and touching spans are absorbed, so
    /// the plan stays sorted and disjoint.
    fn with_candidate(&self, spec: ProjectionSpec, text: &str, candidate: ByteSpan) -> Self {
        let candidate = spec.anchor(text, candidate);
        let first = self
            .spans
            .partition_point(|entry| entry.span.end < candidate.start);
        let absorbed = self.spans[first..]
            .iter()
            .take_while(|entry| entry.span.start <= candidate.end)
            .count();
        let merged = &self.spans[first..first + absorbed];
        let start = merged.first().map_or(candidate.start, |entry| {
            candidate.start.min(entry.span.start)
        });
        let end = merged
            .last()
            .map_or(candidate.end, |entry| candidate.end.max(entry.span.end));

        let mut count = 0;
        let mut released = 0;
        let mut released_bytes = 0;
        let mut cursor = start;
        for entry in merged {
            if cursor < entry.span.start {
                count += spec.count(&text[cursor as usize..entry.span.start as usize]);
            }
            count += entry.count;
            released += entry.count;
            released_bytes += entry.span.end.saturating_sub(entry.span.start);
            cursor = entry.span.end;
        }
        if cursor < end {
            count += spec.count(&text[cursor as usize..end as usize]);
        }

        let mut spans = Vec::with_capacity(self.spans.len() + 1);
        spans.extend_from_slice(&self.spans[..first]);
        spans.push(PlanSpan {
            span: ByteSpan { start, end },
            count,
        });
        spans.extend_from_slice(&self.spans[first + absorbed..]);
        Self {
            spans,
            byte_count: self.byte_count - released_bytes + (end - start),
            visible_count: self.visible_count - released + count,
        }
    }
}

/// Planning keeps a running count of the payload it would render, so a
/// projection tokenizes that payload exactly once, to verify the running total,
/// however many candidates it accepted.
#[cfg(test)]
const MAX_PLAN_FULL_RENDER_COUNTS: usize = 1;

/// One retained span boundary that expansion can still grow from. A frontier
/// that fails is retired: the remaining budget never grows back.
#[derive(Clone, Copy, Debug)]
struct Frontier {
    offset: u64,
    forward: bool,
    active: bool,
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

#[cfg(test)]
thread_local! {
    /// Test-only perturbation of the planned count, so the guard that compares
    /// it against the rendered payload can be proven to fail closed.
    static PLAN_COUNT_BIAS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn planned_count(plan: &SpanPlan) -> u64 {
    let planned = plan.visible_count;
    #[cfg(test)]
    let planned = planned + PLAN_COUNT_BIAS.with(std::cell::Cell::get);
    planned
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

    /// Whether counts of the text before and after `offset` add to the count of
    /// the whole. Byte counts add anywhere; token counts add only on the
    /// offsets `additive_token_offset` accepts.
    fn additive(self, text: &str, offset: usize) -> bool {
        match self.counter {
            Counter::Bytes => true,
            Counter::Cl100k => additive_token_offset(text, offset),
        }
    }

    /// Widens a line-aligned span until both boundaries are additive, so the
    /// plan that retains it can be counted by addition.
    fn anchor(self, text: &str, span: ByteSpan) -> ByteSpan {
        let mut start = span.start as usize;
        while !self.additive(text, start) {
            start = previous_line_start(text, start);
        }
        let mut end = span.end as usize;
        while !self.additive(text, end) {
            end = next_line_start(text, end);
        }
        ByteSpan {
            start: start as u64,
            end: end as u64,
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
    let mut retained = SpanPlan::new(spec, text, &mandatory);
    if retained.visible_count > payload_limit {
        return Err(Failure::new(
            FailureCode::BudgetUnsatisfiable,
            "mandatory facts exceed the projection payload budget",
        ));
    }

    for candidate in analysis.candidates {
        retained = accept_candidate(spec, source.len(), text, retained, candidate, payload_limit);
    }

    if retained.spans.is_empty() {
        if payload_limit > 0 {
            let (prefix_end, prefix_count) =
                fitting_prefix_validated(text, spec, payload_limit, original_count);
            if prefix_end > 0 {
                retained = SpanPlan::prefix(
                    ByteSpan {
                        start: 0,
                        end: prefix_end as u64,
                    },
                    prefix_count,
                );
            }
        }
    } else {
        retained = fill_payload_budget(spec, source.len(), text, retained, payload_limit);
    }

    // The single full-render tokenization of the projection: it verifies the
    // count planning carried, rather than producing it.
    let ranges = retained.ranges();
    let visible = render(source, &ranges)?;
    #[cfg(test)]
    record_full_render_count_evaluation();
    let visible_count = spec.count(&visible);
    if visible_count != planned_count(&retained) {
        return Err(Failure::new(
            FailureCode::InvariantBreach,
            "planned count disagreed with the rendered payload",
        ));
    }
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
        omitted_spans: complement(source.len(), &ranges),
        retained_spans: ranges,
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

fn accept_candidate(
    spec: ProjectionSpec,
    source_len: usize,
    text: &str,
    plan: SpanPlan,
    candidate: ByteSpan,
    payload_limit: u64,
) -> SpanPlan {
    let proposed = plan.with_candidate(spec, text, candidate);
    if receipt_shape_fits(source_len, &proposed.spans) && proposed.visible_count <= payload_limit {
        proposed
    } else {
        plan
    }
}

/// Spends whatever payload budget selection left by growing the retained spans
/// outward one line group at a time, alternating between frontiers so the
/// visible payload keeps both the head and the tail of the observation.
fn fill_payload_budget(
    spec: ProjectionSpec,
    source_len: usize,
    text: &str,
    mut plan: SpanPlan,
    payload_limit: u64,
) -> SpanPlan {
    let mut frontiers = plan
        .spans
        .iter()
        .flat_map(|entry| {
            [
                Frontier {
                    offset: entry.span.end,
                    forward: true,
                    active: true,
                },
                Frontier {
                    offset: entry.span.start,
                    forward: false,
                    active: true,
                },
            ]
        })
        .collect::<Vec<_>>();

    loop {
        let mut progressed = false;
        for frontier in &mut frontiers {
            if !frontier.active {
                continue;
            }
            let Some(target) = growth_target(text, *frontier) else {
                frontier.active = false;
                continue;
            };
            let proposed = plan.with_candidate(spec, text, target);
            if proposed.visible_count > payload_limit
                || !receipt_shape_fits(source_len, &proposed.spans)
            {
                frontier.active = false;
                continue;
            }
            frontier.offset = grown_boundary(&proposed, target, frontier.forward);
            plan = proposed;
            progressed = true;
        }
        if !progressed {
            return plan;
        }
    }
}

fn growth_target(text: &str, frontier: Frontier) -> Option<ByteSpan> {
    let offset = frontier.offset as usize;
    if frontier.forward {
        (offset < text.len()).then(|| ByteSpan {
            start: frontier.offset,
            end: next_line_start(text, offset) as u64,
        })
    } else {
        (offset > 0).then(|| ByteSpan {
            start: previous_line_start(text, offset) as u64,
            end: frontier.offset,
        })
    }
}

/// The outer boundary of the span that absorbed a growth target, which is where
/// that frontier resumes after spans merged.
fn grown_boundary(plan: &SpanPlan, target: ByteSpan, forward: bool) -> u64 {
    let probe = if forward {
        target.start
    } else {
        target.end.saturating_sub(1)
    };
    plan.spans
        .iter()
        .find(|entry| entry.span.start <= probe && probe < entry.span.end)
        .map_or_else(
            || if forward { target.end } else { target.start },
            |entry| {
                if forward {
                    entry.span.end
                } else {
                    entry.span.start
                }
            },
        )
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

fn previous_line_start(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.saturating_sub(1)]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1)
}

fn next_line_start(text: &str, offset: usize) -> usize {
    text.as_bytes()[offset..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(text.len(), |index| offset + index + 1)
}

/// cl100k joins a line break with the whitespace run that the next line break
/// closes, so token counts add across a line boundary only when the following
/// line reaches a non-whitespace character first. The start and the end of the
/// text are additive by definition.
fn additive_token_offset(text: &str, offset: usize) -> bool {
    if offset == 0 || offset == text.len() {
        return true;
    }
    if !text.is_char_boundary(offset) || text.as_bytes()[offset - 1] != b'\n' {
        return false;
    }
    for character in text[offset..].chars() {
        if character == '\n' || character == '\r' {
            return false;
        }
        if !character.is_whitespace() {
            return true;
        }
    }
    true
}

/// Selection builds sorted disjoint plans by construction; the fuzz harness and
/// the planning tests still normalize arbitrary span sets.
#[cfg(any(test, feature = "fuzzing"))]
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

fn complement_len(source_len: usize, retained: &[PlanSpan]) -> usize {
    let mut omitted = 0;
    let mut cursor = 0_u64;
    for entry in retained {
        if entry.span.start > cursor {
            omitted += 1;
        }
        cursor = entry.span.end;
    }
    if cursor < source_len as u64 {
        omitted += 1;
    }
    omitted
}

fn receipt_shape_fits(source_len: usize, retained: &[PlanSpan]) -> bool {
    retained.len() <= MAX_RECEIPT_SPANS && complement_len(source_len, retained) <= MAX_RECEIPT_SPANS
}

/// The longest prefix that fits, with its exact count, so the fallback plan
/// needs no separate accounting pass. `whole` is the caller's already measured
/// count of the full text.
fn fitting_prefix_validated(
    text: &str,
    spec: ProjectionSpec,
    limit: u64,
    whole: u64,
) -> (usize, u64) {
    let mut low = 0_usize;
    let mut low_count = 0_u64;
    let mut high = text.len();
    if whole <= limit {
        return (high, whole);
    }
    while low + 1 < high {
        let mut middle = low + (high - low) / 2;
        while middle > low && !text.is_char_boundary(middle) {
            middle -= 1;
        }
        if middle == low {
            let Some(next) = text[low..].chars().next() else {
                return (low, low_count);
            };
            middle = low + next.len_utf8();
            if middle >= high {
                return (low, low_count);
            }
        }
        let count = spec.count(&text[..middle]);
        if count <= limit {
            low = middle;
            low_count = count;
        } else {
            high = middle;
        }
    }
    (low, low_count)
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
        .map(|spec| fitting_prefix_validated(text, spec, limit, spec.count(text)).0)
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
