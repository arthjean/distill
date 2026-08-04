use crate::{
    contract::MAX_RECEIPT_SPANS,
    types::{
        AggregateSpan, Budget, ByteSpan, CL100K_PROFILE, CountUnit, Failure, FailureCode, Fidelity,
    },
};
use aggregate::{Aggregation, Templates};
use shape::{LineClass, Shape};
use tiktoken_rs::cl100k_base_singleton;

mod aggregate;
mod focus;
mod retrieval;
mod shape;

pub(crate) use focus::Focus;
pub(crate) use retrieval::Selection;

/// The reducer work limit. It applies to mandatory facts, and once per reason a
/// line can become a candidate: structural rank and focus proximity each admit
/// at most this many, so a focus reaches the whole observation without ever
/// crowding out the structure the shape policy found.
const MAX_REDUCER_SPANS: usize = 256;

/// How deep into each section a distributing shape offers candidates, given how
/// many sections it has.
///
/// The work limit admits candidates in source order, so on a diff of 669
/// changed lines only the first 256 were ever offered, and selection could not
/// reach past the opening third whatever order it then applied. Dividing the
/// same slots by the number of sections spreads them instead: a two-hunk diff
/// still offers every line it has, and a fifty-hunk diff offers the opening of
/// each. Nothing here is tuned to a corpus, and expansion widens the retained
/// spans from whatever the depth admits.
fn section_candidate_depth(sections: u32) -> u32 {
    let sections = sections.max(1);
    (MAX_REDUCER_SPANS as u32 / sections).max(1)
}

/// Counts the sections of a distributing shape, so the candidate depth can be
/// divided among them before any line is ranked.
fn count_sections(text: &str, shape: Shape) -> u32 {
    text.split_inclusive('\n')
        .filter(|line| shape.opens_section(line))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

/// The profile both product surfaces send: the policy is derived from the
/// detected shape of the observation rather than declared by the caller.
pub const AUTO_PROFILE: &str = "auto/v1";

#[derive(Clone, Copy, Debug)]
struct Policy {
    id: &'static str,
    /// The shape this identifier fixes, or `None` when the shape is detected.
    shape: Option<Shape>,
}

/// The preservation profiles the contract accepts.
///
/// The first group names the observation shapes the projector recognizes. The
/// second is the retired identifier set: `distill.context/v3` keeps every one of
/// them accepted by resolving it to the shape policy it used to approximate, so
/// no caller breaks and no literal fixture needle survives.
const POLICIES: &[Policy] = &[
    Policy {
        id: AUTO_PROFILE,
        shape: None,
    },
    Policy {
        id: "build-output/v1",
        shape: Some(Shape::BuildOutput),
    },
    Policy {
        id: "test-output/v1",
        shape: Some(Shape::TestOutput),
    },
    Policy {
        id: "typecheck-lint/v1",
        shape: Some(Shape::TypecheckLint),
    },
    Policy {
        id: "stack-trace/v1",
        shape: Some(Shape::StackTrace),
    },
    Policy {
        id: "unified-diff/v1",
        shape: Some(Shape::UnifiedDiff),
    },
    Policy {
        id: "api-json/v1",
        shape: Some(Shape::ApiJson),
    },
    Policy {
        id: "source-file/v1",
        shape: Some(Shape::SourceFile),
    },
    Policy {
        id: "terminal-log/v1",
        shape: Some(Shape::TerminalLog),
    },
    // Retired identifiers, resolved by the v3 contract.
    Policy {
        id: "plain-text/v1",
        shape: Some(Shape::TerminalLog),
    },
    Policy {
        id: "build-log/v1",
        shape: Some(Shape::BuildOutput),
    },
    Policy {
        id: "test-log/v1",
        shape: Some(Shape::TestOutput),
    },
    Policy {
        id: "diagnostic/v1",
        shape: Some(Shape::TypecheckLint),
    },
    Policy {
        id: "diff/v1",
        shape: Some(Shape::UnifiedDiff),
    },
    Policy {
        id: "json/v1",
        shape: Some(Shape::ApiJson),
    },
    Policy {
        id: "source-code/v1",
        shape: Some(Shape::SourceFile),
    },
    Policy {
        id: "unicode/v1",
        shape: Some(Shape::TerminalLog),
    },
    Policy {
        id: "binary/v1",
        shape: Some(Shape::TerminalLog),
    },
    Policy {
        id: "untrusted-text/v1",
        shape: Some(Shape::TerminalLog),
    },
    Policy {
        id: "none/v1",
        shape: Some(Shape::TerminalLog),
    },
];

#[derive(Debug)]
struct Analysis {
    mandatory: Vec<ByteSpan>,
    candidates: Vec<Candidate>,
    aggregation: Aggregation,
}

/// One optional line the plan may buy, with everything ordering needs to rank
/// it. Mandatory lines are not candidates: they are already retained.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    span: ByteSpan,
    /// The focus terms the line carries. Empty without a focus.
    matched: focus::Match,
    /// How many discriminating focus terms the line carries, resolved once the
    /// whole observation has been seen.
    lexical: u32,
    /// The rank the shape policy gave the line: a preferred line outranks the
    /// boundary spans every observation offers.
    structural: u8,
    /// Where the line sits inside its section, for a shape whose sections are
    /// independent. `u32::MAX` marks a line that carries no section rank, which
    /// keeps boundary spans at the end of the order.
    section_rank: u32,
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
    /// What the annotations of the collapsed runs between these spans add to
    /// the payload. It is the only visible content that is not source bytes.
    annotation_count: u64,
}

impl SpanPlan {
    fn new(
        spec: ProjectionSpec,
        text: &str,
        spans: &[ByteSpan],
        aggregation: &Aggregation,
    ) -> Self {
        spans.iter().fold(Self::default(), |plan, span| {
            plan.with_candidate(spec, text, *span, aggregation)
        })
    }

    fn prefix(span: ByteSpan, count: u64) -> Self {
        Self {
            spans: vec![PlanSpan { span, count }],
            byte_count: span.end.saturating_sub(span.start),
            visible_count: count,
            annotation_count: 0,
        }
    }

    fn ranges(&self) -> Vec<ByteSpan> {
        self.spans.iter().map(|entry| entry.span).collect()
    }

    /// Everything the plan would render: the retained source plus the
    /// annotation of every run it collapsed.
    fn payload_count(&self) -> u64 {
        self.visible_count.saturating_add(self.annotation_count)
    }

    /// Merges one candidate into the plan, counting only the source the plan
    /// did not already cover. Overlapping and touching spans are absorbed, so
    /// the plan stays sorted and disjoint.
    fn with_candidate(
        &self,
        spec: ProjectionSpec,
        text: &str,
        candidate: ByteSpan,
        aggregation: &Aggregation,
    ) -> Self {
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
        let annotation_count = aggregation.annotation_count(spec, &spans);
        Self {
            spans,
            byte_count: self.byte_count - released_bytes + (end - start),
            visible_count: self.visible_count - released + count,
            annotation_count,
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
    let planned = plan.payload_count();
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
    /// The profile identifier the request named, which the receipt keeps
    /// distinct from the policy that ran.
    requested: &'static str,
    /// The shape in force. It is the profile's own shape, or the line-structured
    /// fallback until detection resolves `auto/v1`.
    shape: Shape,
    /// Whether the shape still has to be detected from the observation.
    detect: bool,
    /// Whether template aggregation may collapse redundant lines. Bounded
    /// retrieval clears it: a caller who asks for a region wants that region.
    aggregate: bool,
    counter: Counter,
    total_visible_limit: u64,
    reserved_envelope: u64,
    /// The largest retained or omitted partition the plan may return. Selected
    /// projection lowers it, because translating spans back into source offsets
    /// can split one planned span at every region boundary.
    span_ceiling: usize,
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
            requested: policy.id,
            shape: policy.shape.unwrap_or(Shape::TerminalLog),
            detect: policy.shape.is_none(),
            aggregate: true,
            counter,
            total_visible_limit: budget.total_visible_limit,
            reserved_envelope: budget.reserved_envelope,
            span_ceiling: MAX_RECEIPT_SPANS,
        })
    }

    /// Resolves the policy against the observation itself. A profile that names
    /// a shape keeps it; `auto/v1` derives it from a bounded prefix of the text,
    /// and falls back to the line-structured policy when there is no text to
    /// inspect. Resolution is idempotent.
    fn resolved(self, text: Option<&str>) -> Self {
        if !self.detect {
            return self;
        }
        Self {
            shape: text.map_or(Shape::TerminalLog, shape::detect),
            detect: false,
            ..self
        }
    }

    /// Reserves room for the spans that translation back into source offsets
    /// can add, so a selected projection cannot return an unpersistable receipt.
    fn with_region_headroom(self, regions: usize) -> Self {
        Self {
            span_ceiling: MAX_RECEIPT_SPANS.saturating_sub(regions).max(1),
            ..self
        }
    }

    /// Bounded retrieval returns the region the caller selected, verbatim.
    fn without_aggregation(self) -> Self {
        Self {
            aggregate: false,
            ..self
        }
    }

    fn aggregates(self) -> bool {
        self.aggregate && self.shape.aggregates()
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

    /// The profile the request named.
    pub(crate) fn profile(self) -> &'static str {
        self.requested
    }

    /// The policy that ran, which names the shape it was derived from.
    pub(crate) fn applied_profile(self) -> &'static str {
        self.shape.profile()
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
    /// The policy that ran, derived from the detected shape.
    pub applied_profile: &'static str,
    /// The runs the payload states as collapsed. They stay outside the span
    /// partition: every one of them is inside `omitted_spans`.
    pub aggregates: Vec<AggregateSpan>,
}

pub(crate) fn project_validated(
    source: &[u8],
    spec: ProjectionSpec,
    focus: Option<&Focus>,
) -> Result<Projection, Failure> {
    let payload_limit = spec.payload_limit();
    if payload_limit == 0 && !source.is_empty() {
        return Err(Failure::new(
            FailureCode::BudgetUnsatisfiable,
            "nonempty content cannot satisfy a zero payload budget",
        ));
    }
    if let Ok(text) = std::str::from_utf8(source) {
        let spec = spec.resolved(Some(text));
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
                mandatory_fact_ids: mandatory_spans(source, spec.shape)?
                    .into_iter()
                    .map(|span| fact_id(spec.applied_profile(), span))
                    .collect(),
                applied_profile: spec.applied_profile(),
                aggregates: Vec::new(),
            });
        }
        return project_text(source, text, spec, payload_limit, original_count, focus);
    }
    project_binary(source, spec.resolved(None), payload_limit)
}

/// Projects a bounded selection of a committed artifact through the same
/// planner, then translates the retained spans back into offsets in the
/// original source, so the receipt still partitions the artifact exactly and
/// still accounts for everything the caller cannot see.
pub(crate) fn project_selection(
    source: &[u8],
    selection: &Selection,
    spec: ProjectionSpec,
    focus: Option<&Focus>,
) -> Result<Projection, Failure> {
    let text = std::str::from_utf8(source).map_err(|_| {
        Failure::new(
            FailureCode::InvalidRequest,
            "artifact selector requires a UTF-8 source",
        )
    })?;
    let spec = spec.resolved(Some(text)).without_aggregation();
    let original_count = spec.count(text);
    let regions = retrieval::select_regions(text, selection);
    if regions.is_empty() {
        return Ok(Projection {
            visible: String::new(),
            original_count,
            visible_count: 0,
            fidelity: Fidelity::Extractive,
            retained_spans: Vec::new(),
            omitted_spans: complement(source.len(), &[]),
            mandatory_fact_ids: Vec::new(),
            applied_profile: spec.applied_profile(),
            aggregates: Vec::new(),
        });
    }

    let selected = retrieval::render_regions(source, &regions);
    let inner = project_validated(&selected, spec.with_region_headroom(regions.len()), focus)?;
    let fidelity = match inner.fidelity {
        Fidelity::Exact if covers_source(&regions, source.len()) => Fidelity::Exact,
        Fidelity::Exact | Fidelity::Extractive => Fidelity::Extractive,
        Fidelity::Encoded | Fidelity::MetadataOnly => {
            return Err(Failure::new(
                FailureCode::InvariantBreach,
                "line-aligned selection produced non-UTF-8 content",
            ));
        }
    };

    let retained_spans = retrieval::translate_spans(&regions, &inner.retained_spans);
    let omitted_spans = complement(source.len(), &retained_spans);
    if retained_spans.len() > MAX_RECEIPT_SPANS || omitted_spans.len() > MAX_RECEIPT_SPANS {
        return Err(Failure::new(
            FailureCode::InvariantBreach,
            "selected receipt exceeds the persisted span limit",
        ));
    }
    Ok(Projection {
        visible: inner.visible,
        original_count,
        visible_count: inner.visible_count,
        fidelity,
        retained_spans,
        omitted_spans,
        mandatory_fact_ids: mandatory_spans(&selected, spec.shape)?
            .into_iter()
            .flat_map(|span| retrieval::translate_spans(&regions, &[span]))
            .map(|span| fact_id(spec.applied_profile(), span))
            .collect(),
        applied_profile: spec.applied_profile(),
        aggregates: Vec::new(),
    })
}

fn covers_source(regions: &[ByteSpan], source_len: usize) -> bool {
    matches!(regions, [only] if only.start == 0 && only.end == source_len as u64)
}

fn project_text(
    source: &[u8],
    text: &str,
    spec: ProjectionSpec,
    payload_limit: u64,
    original_count: u64,
    focus: Option<&Focus>,
) -> Result<Projection, Failure> {
    let analysis = analyze(source, text, spec, focus)?;
    let mandatory = analysis.mandatory;
    let aggregation = &analysis.aggregation;
    let mut retained = SpanPlan::new(spec, text, &mandatory, aggregation);
    if retained.payload_count() > payload_limit {
        return Err(Failure::new(
            FailureCode::BudgetUnsatisfiable,
            "mandatory facts exceed the projection payload budget",
        ));
    }

    for candidate in analysis.candidates {
        retained = accept_candidate(
            spec,
            source.len(),
            text,
            retained,
            candidate.span,
            payload_limit,
            aggregation,
        );
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
        // Collapsed redundancy is stepped over first, so the budget buys
        // distinct content. Only when that pass runs out of source rather than
        // out of budget does the remainder get spent on the collapsed lines
        // themselves: unspent budget helps nobody, but budget spent on
        // redundancy is exactly what aggregation exists to prevent.
        let filled = fill_payload_budget(
            spec,
            source.len(),
            text,
            retained,
            payload_limit,
            aggregation,
            Growth::OverCollapsed,
        );
        retained = if filled.budget_bound {
            filled.plan
        } else {
            fill_payload_budget(
                spec,
                source.len(),
                text,
                filled.plan,
                payload_limit,
                aggregation,
                Growth::Contiguous,
            )
            .plan
        };
    }

    // The single full-render tokenization of the projection: it verifies the
    // count planning carried, rather than producing it.
    let ranges = retained.ranges();
    let visible = render(source, &ranges, aggregation)?;
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
        aggregates: aggregation.collapsed(&ranges),
        retained_spans: ranges,
        mandatory_fact_ids: mandatory
            .iter()
            .map(|span| fact_id(spec.applied_profile(), *span))
            .collect(),
        applied_profile: spec.applied_profile(),
    })
}

fn project_binary(
    source: &[u8],
    spec: ProjectionSpec,
    payload_limit: u64,
) -> Result<Projection, Failure> {
    let encoded = encode_binary(source);
    let mandatory = mandatory_spans(source, spec.shape)?;
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
                .map(|span| fact_id(spec.applied_profile(), *span))
                .collect(),
            applied_profile: spec.applied_profile(),
            aggregates: Vec::new(),
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
        applied_profile: spec.applied_profile(),
        aggregates: Vec::new(),
    })
}

/// The mandatory facts of a source that is not planned line by line: an exact
/// projection, which keeps everything, and an encoded binary one, which keeps
/// nothing it could reduce.
fn mandatory_spans(source: &[u8], shape: Shape) -> Result<Vec<ByteSpan>, Failure> {
    let mut spans = Vec::new();
    for span in line_spans(source) {
        let line = String::from_utf8_lossy(&source[span.start as usize..span.end as usize]);
        let lowercase = line.to_ascii_lowercase();
        if shape.classify(&line, &lowercase) == LineClass::Mandatory {
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

/// Ranks every line of the source under the resolved shape policy, scores it
/// against the caller's focus, and groups the redundant ones in the same pass.
fn analyze(
    source: &[u8],
    text: &str,
    spec: ProjectionSpec,
    focus: Option<&Focus>,
) -> Result<Analysis, Failure> {
    let mut mandatory = Vec::new();
    let mut candidates = Vec::new();
    let mut first = None;
    let mut last = None;
    let mut templates = Templates::new(spec.aggregates());
    let mut matched_lines = focus::Counts::default();
    let mut lines = 0_u64;
    let mut structural_count = 0_usize;
    let mut focused_count = 0_usize;
    let mut section_rank = 0_u32;
    let section_depth = if spec.shape.distributes() {
        section_candidate_depth(count_sections(text, spec.shape))
    } else {
        u32::MAX
    };
    for span in line_spans(source) {
        lines += 1;
        first.get_or_insert(span);
        last = Some(span);
        let line = &text[span.start as usize..span.end as usize];
        let lowercase = line.to_ascii_lowercase();
        let class = spec.shape.classify(line, &lowercase);
        let matched = focus.map_or(0, |focus| focus.matches(&lowercase));
        focus::accumulate(&mut matched_lines, matched);
        // A shape whose sections are independent ranks its lines within their
        // section, so selection can spread across all of them instead of
        // filling the first ones.
        if spec.shape.distributes() {
            if spec.shape.opens_section(line) {
                section_rank = 0;
            } else {
                section_rank = section_rank.saturating_add(1);
            }
        }
        match class {
            LineClass::Mandatory => {
                if mandatory.len() == MAX_REDUCER_SPANS {
                    return Err(Failure::new(
                        FailureCode::ResourceExhausted,
                        "mandatory fact count exceeds the reducer work limit",
                    ));
                }
                mandatory.push(span);
            }
            // A focus promotes a line the shape policy ranked ordinary: what
            // answers the caller's question is rarely what orders the output.
            // The work limit applies once per reason a line can qualify, so a
            // focus reaches the whole observation rather than the prefix whose
            // structure already filled the table.
            LineClass::Preferred | LineClass::Ordinary => {
                let structural = u8::from(class == LineClass::Preferred);
                let within_section_depth =
                    !spec.shape.distributes() || section_rank <= section_depth;
                let by_structure =
                    structural > 0 && within_section_depth && structural_count < MAX_REDUCER_SPANS;
                let by_focus = matched > 0 && focused_count < MAX_REDUCER_SPANS;
                if by_structure || by_focus {
                    structural_count += usize::from(by_structure);
                    focused_count += usize::from(by_focus);
                    candidates.push(Candidate {
                        span,
                        matched,
                        lexical: 0,
                        structural,
                        section_rank: if spec.shape.distributes() {
                            section_rank
                        } else {
                            0
                        },
                    });
                }
            }
        }
        templates.observe(span, line, class);
    }
    // Which focus terms actually locate an answer is only knowable once the
    // whole observation has been ranked, so scoring resolves here. A line the
    // focus promoted but no discriminating term reached is dropped again: a
    // focus whose terms say nothing about this observation leaves selection
    // exactly where it was.
    if let Some(focus) = focus {
        let discriminating = focus.discriminating(&matched_lines, lines);
        for candidate in &mut candidates {
            candidate.lexical = focus::score(candidate.matched, discriminating);
        }
        candidates.retain(|candidate| candidate.structural > 0 || candidate.lexical > 0);
    }
    // The boundaries of an observation are always offered: they are what an
    // agent reads first when it cannot read everything. They are offered, never
    // ranked, so a focus cannot displace them from the end of the order.
    for boundary in [first, last].into_iter().flatten() {
        candidates.push(Candidate {
            span: boundary,
            matched: 0,
            lexical: 0,
            structural: 0,
            section_rank: u32::MAX,
        });
    }
    order_candidates(&mut candidates, spec.shape.distributes());
    Ok(Analysis {
        mandatory,
        candidates,
        aggregation: templates.finish([first, last]),
    })
}

/// Orders candidates by the deterministic score selection plans them in.
///
/// Lexical proximity to the focus ranks first, the structural rank of the shape
/// policy second, and source position last, so a line that answers the question
/// outranks a line that merely opens a section, and equal scores always resolve
/// the same way. A focus no line carries scores nothing, and an observation read
/// without one scores nothing either: both keep the source order the shape
/// policy produced, so ordering can only take effect where it discriminates.
fn order_candidates(candidates: &mut [Candidate], distributes: bool) {
    if !distributes && candidates.iter().all(|candidate| candidate.lexical == 0) {
        return;
    }
    candidates.sort_by_key(|candidate| {
        (
            std::cmp::Reverse(candidate.lexical),
            std::cmp::Reverse(candidate.structural),
            candidate.section_rank,
            candidate.span.start,
            candidate.span.end,
        )
    });
}

#[allow(clippy::too_many_arguments)]
fn accept_candidate(
    spec: ProjectionSpec,
    source_len: usize,
    text: &str,
    plan: SpanPlan,
    candidate: ByteSpan,
    payload_limit: u64,
    aggregation: &Aggregation,
) -> SpanPlan {
    let proposed = plan.with_candidate(spec, text, candidate, aggregation);
    if receipt_shape_fits(spec, source_len, &proposed.spans) {
        return if proposed.payload_count() <= payload_limit {
            proposed
        } else {
            plan
        };
    }
    // The receipt span ceiling is reached. Bridging the candidate to its
    // nearest retained neighbour merges the fragment instead of dropping it,
    // which keeps both partitions inside the ceiling.
    let Some(bridged) = bridge(&plan, candidate) else {
        return plan;
    };
    let proposed = plan.with_candidate(spec, text, bridged, aggregation);
    if receipt_shape_fits(spec, source_len, &proposed.spans)
        && proposed.payload_count() <= payload_limit
    {
        proposed
    } else {
        plan
    }
}

/// The span that joins a candidate to its nearest retained neighbour. Ties
/// resolve to the preceding neighbour so selection stays deterministic.
fn bridge(plan: &SpanPlan, candidate: ByteSpan) -> Option<ByteSpan> {
    let mut nearest = None;
    let mut distance = u64::MAX;
    for entry in &plan.spans {
        let (bridged, gap) = if entry.span.end <= candidate.start {
            (
                ByteSpan {
                    start: entry.span.end,
                    end: candidate.end,
                },
                candidate.start - entry.span.end,
            )
        } else if candidate.end <= entry.span.start {
            (
                ByteSpan {
                    start: candidate.start,
                    end: entry.span.start,
                },
                entry.span.start - candidate.end,
            )
        } else {
            continue;
        };
        if gap < distance {
            distance = gap;
            nearest = Some(bridged);
        }
    }
    nearest
}

/// How expansion treats the runs aggregation collapsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Growth {
    /// Step over a collapsed run and keep its annotation, so the budget buys
    /// distinct content instead of redundancy.
    OverCollapsed,
    /// Grow through everything. Collapsing frees budget; when nothing distinct
    /// is left to spend it on, the collapsed lines themselves are what remains.
    Contiguous,
}

/// A filled plan, with what stopped it: a pass that ran out of budget has
/// nothing left to spend, while a pass that ran out of source may still have.
struct Filled {
    plan: SpanPlan,
    budget_bound: bool,
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
    aggregation: &Aggregation,
    growth: Growth,
) -> Filled {
    let mut budget_bound = false;
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
            let Some(target) = growth_target(text, *frontier, aggregation, growth) else {
                frontier.active = false;
                continue;
            };
            let proposed = plan.with_candidate(spec, text, target, aggregation);
            if proposed.payload_count() > payload_limit
                || !receipt_shape_fits(spec, source_len, &proposed.spans)
            {
                budget_bound = true;
                frontier.active = false;
                continue;
            }
            frontier.offset = grown_boundary(&proposed, target, frontier.forward);
            plan = proposed;
            progressed = true;
        }
        if !progressed {
            return Filled { plan, budget_bound };
        }
    }
}

/// The next whole line a frontier can grow onto, stepping over a collapsed run
/// rather than retaining it. Runs are maximal, so one step always lands on a
/// line the plan may keep.
fn growth_target(
    text: &str,
    frontier: Frontier,
    aggregation: &Aggregation,
    growth: Growth,
) -> Option<ByteSpan> {
    let collapsed = |offset| {
        (growth == Growth::OverCollapsed)
            .then(|| {
                if frontier.forward {
                    aggregation.run_from(offset).map(|run| run.end)
                } else {
                    aggregation.run_to(offset).map(|run| run.start)
                }
            })
            .flatten()
            .unwrap_or(offset)
    };
    if frontier.forward {
        let start = collapsed(frontier.offset);
        let offset = start as usize;
        (offset < text.len()).then(|| ByteSpan {
            start,
            end: next_line_start(text, offset) as u64,
        })
    } else {
        let end = collapsed(frontier.offset);
        let offset = end as usize;
        (offset > 0).then(|| ByteSpan {
            start: previous_line_start(text, offset) as u64,
            end,
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

fn render(source: &[u8], spans: &[ByteSpan], aggregation: &Aggregation) -> Result<String, Failure> {
    let mut visible = Vec::new();
    let mut previous_end = None;
    for span in spans {
        if let Some(previous_end) = previous_end
            && let Some(annotation) = aggregation.annotation_before(previous_end, span.start)
        {
            visible.extend_from_slice(annotation.as_bytes());
        }
        previous_end = Some(span.end);
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

fn receipt_shape_fits(spec: ProjectionSpec, source_len: usize, retained: &[PlanSpan]) -> bool {
    retained.len() <= spec.span_ceiling && complement_len(source_len, retained) <= spec.span_ceiling
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
    // The executed profile: the shape of untrusted bytes is detected, never
    // declared, so the classifier is fuzzed with everything else. The focus is
    // drawn from the same untrusted bytes, because it is data on exactly the
    // same footing as the source it orders.
    let focus = String::from_utf8_lossy(&data[..data.len().min(crate::contract::MAX_FOCUS_BYTES)])
        .into_owned();
    let focus = Focus::new(&focus);
    let result = ProjectionSpec::new(AUTO_PROFILE, &budget)
        .and_then(|spec| project_validated(data, spec, Some(&focus)));
    if matches!(
        result,
        Err(Failure {
            code: FailureCode::InvariantBreach,
            ..
        })
    ) {
        std::process::abort();
    }

    // The same untrusted bytes, read through a bounded selector whose literal
    // pattern is itself drawn from them.
    let pattern = String::from_utf8_lossy(
        &data[..data.len().min(crate::contract::MAX_SELECTOR_PATTERN_BYTES)],
    )
    .into_owned();
    if !pattern.is_empty()
        && let Ok(spec) = ProjectionSpec::new(AUTO_PROFILE, &budget)
        && matches!(
            project_selection(
                data,
                &Selection::Pattern {
                    pattern,
                    before_lines: 2,
                    after_lines: 2,
                    max_matches: 8,
                },
                spec,
                Some(&focus),
            ),
            Err(Failure {
                code: FailureCode::InvariantBreach,
                ..
            })
        )
    {
        std::process::abort();
    }

    let mut spans = line_spans(data).take(MAX_REDUCER_SPANS).collect();
    normalize_spans(&mut spans);
    let _omitted = complement(data.len(), &spans);
}

#[cfg(test)]
fn count(text: &str, budget: &Budget) -> Result<u64, Failure> {
    ProjectionSpec::new(AUTO_PROFILE, budget).map(|spec| spec.count(text))
}

#[cfg(test)]
fn fitting_prefix(text: &str, budget: &Budget, limit: u64) -> Result<usize, Failure> {
    ProjectionSpec::new(AUTO_PROFILE, budget)
        .map(|spec| fitting_prefix_validated(text, spec, limit, spec.count(text)).0)
}

#[cfg(test)]
fn project(source: &[u8], budget: &Budget, profile: &str) -> Result<Projection, Failure> {
    project_focused(source, budget, profile, None)
}

#[cfg(test)]
fn project_focused(
    source: &[u8],
    budget: &Budget,
    profile: &str,
    focus: Option<&str>,
) -> Result<Projection, Failure> {
    let focus = focus.map(Focus::new);
    ProjectionSpec::new(profile, budget)
        .and_then(|spec| project_validated(source, spec, focus.as_ref()))
}

#[cfg(test)]
fn project_with_metrics(
    source: &[u8],
    budget: &Budget,
    profile: &str,
    focus: Option<&str>,
    metrics: &mut PlanningMetrics,
) -> Result<Projection, Failure> {
    FULL_RENDER_COUNT_EVALUATIONS.with(|count| count.set(0));
    let result = project_focused(source, budget, profile, focus);
    metrics.full_render_count_evaluations =
        FULL_RENDER_COUNT_EVALUATIONS.with(std::cell::Cell::get);
    result
}

#[cfg(test)]
#[path = "projection/tests.rs"]
mod tests;
