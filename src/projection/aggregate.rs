//! Template aggregation for redundant machine output.
//!
//! Repeated tool output spends the budget that distinct facts need. This module
//! groups lines that differ only in variable literals, keeps the first
//! occurrence of every template, and marks the later ones as suppressible. The
//! projector then grows across the suppressed runs instead of retaining them,
//! and states each collapsed run in the visible payload with its line count.
//!
//! The grouping follows Drain as a design reference: mask value-shaped tokens,
//! bucket by token count and leading token, and accept a template when at least
//! half of the positions agree. It is a bounded in-crate implementation, not a
//! port, and every bound below is a guard rather than a tuning parameter.

use super::{ProjectionSpec, shape::LineClass};
use crate::types::ByteSpan;
use std::{cell::Cell, collections::HashMap};

/// Aggregation only serves budgets, so a source with more lines than this is
/// left unaggregated rather than paying an unbounded grouping pass.
const MAX_AGGREGATION_LINES: usize = 50_000;
/// The number of distinct templates the pass may learn, globally and per
/// bucket. Beyond either cap it stops learning and keeps matching, so behavior
/// stays deterministic instead of degrading with input size.
const MAX_TEMPLATES: usize = 4_096;
const MAX_BUCKET_TEMPLATES: usize = 64;
/// Tokens compared per line. A line longer than this matches on its head.
const MAX_TEMPLATE_TOKENS: usize = 32;
/// The shortest run worth collapsing. An annotation costs more than the one or
/// two lines it would replace, so short repeats stay verbatim.
const MIN_COLLAPSED_LINES: u64 = 3;
/// The token that stands for a position whose value varies.
const WILDCARD: &str = "\u{0}*";

/// One run of consecutive suppressed lines, with the annotation that replaces
/// it in the visible payload.
#[derive(Debug)]
struct Run {
    span: ByteSpan,
    lines: u64,
    /// The annotation count under the projection counter, resolved once.
    count: Cell<Option<u64>>,
}

/// The suppression map of one source: sorted, disjoint runs of lines whose
/// template already appeared earlier.
#[derive(Debug, Default)]
pub(super) struct Aggregation {
    runs: Vec<Run>,
}

/// Accumulates templates while the caller walks the source once.
pub(super) struct Templates<'a> {
    buckets: HashMap<(usize, &'a str), Vec<Vec<&'a str>>>,
    learned: usize,
    masked: Vec<&'a str>,
    runs: Vec<Run>,
    lines: usize,
    active: bool,
}

impl<'a> Templates<'a> {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            buckets: HashMap::new(),
            learned: 0,
            masked: Vec::with_capacity(MAX_TEMPLATE_TOKENS),
            runs: Vec::new(),
            lines: 0,
            active: enabled,
        }
    }

    /// Offers one line to the grouping pass and reports whether it repeats a
    /// template the pass already retained. Only ordinary lines are ever
    /// suppressed: a line the shape policy ranks is a fact, not redundancy.
    pub(super) fn observe(&mut self, span: ByteSpan, line: &'a str, class: LineClass) {
        if !self.active {
            return;
        }
        self.lines += 1;
        if self.lines > MAX_AGGREGATION_LINES {
            self.active = false;
            self.runs.clear();
            return;
        }
        if !self.matches(line) || class != LineClass::Ordinary {
            return;
        }
        match self.runs.last_mut() {
            Some(last) if last.span.end == span.start => {
                last.span.end = span.end;
                last.lines += 1;
            }
            _ => self.runs.push(Run {
                span,
                lines: 1,
                count: Cell::new(None),
            }),
        }
    }

    /// Whether the line repeats a known template, learning it otherwise.
    fn matches(&mut self, line: &'a str) -> bool {
        self.masked.clear();
        self.masked.extend(
            line.split_ascii_whitespace()
                .take(MAX_TEMPLATE_TOKENS)
                .map(mask),
        );
        let Some(first) = self.masked.first().copied() else {
            return false;
        };
        let bucket = self.buckets.entry((self.masked.len(), first)).or_default();
        for template in bucket.iter_mut() {
            let agreed = template
                .iter()
                .zip(&self.masked)
                .filter(|(left, right)| left == right)
                .count();
            if agreed * 2 < template.len() {
                continue;
            }
            for (position, token) in template.iter_mut().zip(&self.masked) {
                if position != token {
                    *position = WILDCARD;
                }
            }
            return true;
        }
        if bucket.len() < MAX_BUCKET_TEMPLATES && self.learned < MAX_TEMPLATES {
            bucket.push(self.masked.clone());
            self.learned += 1;
        }
        false
    }

    /// Closes the pass, keeping the source boundary lines retainable: they are
    /// the anchors selection always offers, so collapsing them would remove the
    /// head or the tail of the observation instead of its redundancy.
    pub(super) fn finish(mut self, boundaries: [Option<ByteSpan>; 2]) -> Aggregation {
        for boundary in boundaries.into_iter().flatten() {
            release(&mut self.runs, boundary);
        }
        self.runs
            .retain(|run| run.span.end > run.span.start && run.lines >= MIN_COLLAPSED_LINES);
        Aggregation { runs: self.runs }
    }
}

/// Removes one line from the run that covers it, so the line stays retainable.
fn release(runs: &mut [Run], line: ByteSpan) {
    for run in runs.iter_mut() {
        if run.span.start == line.start && line.end <= run.span.end {
            run.span.start = line.end;
            run.lines = run.lines.saturating_sub(1);
        } else if run.span.end == line.end && line.start >= run.span.start {
            run.span.end = line.start;
            run.lines = run.lines.saturating_sub(1);
        }
    }
}

/// Masks a token that carries a value rather than structure.
fn mask(token: &str) -> &str {
    if token.bytes().any(|byte| byte.is_ascii_digit()) {
        WILDCARD
    } else {
        token
    }
}

impl Aggregation {
    /// The run that starts exactly at `offset`, which forward growth steps over.
    pub(super) fn run_from(&self, offset: u64) -> Option<ByteSpan> {
        self.runs
            .binary_search_by_key(&offset, |run| run.span.start)
            .ok()
            .map(|index| self.runs[index].span)
    }

    /// The run that ends exactly at `offset`, which backward growth steps over.
    pub(super) fn run_to(&self, offset: u64) -> Option<ByteSpan> {
        self.runs
            .binary_search_by_key(&offset, |run| run.span.end)
            .ok()
            .map(|index| self.runs[index].span)
    }

    /// The annotation a gap between two retained spans carries.
    ///
    /// Runs are maximal, so two of them are always separated by a line that is
    /// not suppressed. A gap therefore carries an annotation only when it is
    /// exactly one run, and never when ordinary omitted content is mixed in.
    fn run_between(&self, start: u64, end: u64) -> Option<&Run> {
        let index = self
            .runs
            .binary_search_by_key(&start, |run| run.span.start)
            .ok()?;
        let run = self.runs.get(index)?;
        (run.span.end == end).then_some(run)
    }

    /// The count the annotations of a plan add to the payload.
    pub(super) fn annotation_count(&self, spec: ProjectionSpec, spans: &[super::PlanSpan]) -> u64 {
        if self.runs.is_empty() {
            return 0;
        }
        spans
            .windows(2)
            .filter_map(|pair| self.run_between(pair[0].span.end, pair[1].span.start))
            .map(|run| self.count_of(spec, run))
            .sum()
    }

    fn count_of(&self, spec: ProjectionSpec, run: &Run) -> u64 {
        match run.count.get() {
            Some(count) => count,
            None => {
                let count = spec.count(&annotation(run.lines));
                run.count.set(Some(count));
                count
            }
        }
    }

    /// The annotation that precedes a retained span, when the gap that opens
    /// before it is exactly one collapsed run.
    pub(super) fn annotation_before(&self, previous_end: u64, start: u64) -> Option<String> {
        self.run_between(previous_end, start)
            .map(|run| annotation(run.lines))
    }

    /// The collapsed runs a plan reports, in source order, for the receipt.
    pub(super) fn collapsed(&self, spans: &[ByteSpan]) -> Vec<crate::types::AggregateSpan> {
        spans
            .windows(2)
            .filter_map(|pair| self.run_between(pair[0].end, pair[1].start))
            .map(|run| crate::types::AggregateSpan {
                span: run.span,
                lines: run.lines,
            })
            .collect()
    }
}

/// The line that replaces a collapsed run in the visible payload. It is the
/// only text a projection emits that is not a verbatim source slice, and the
/// receipt states each one.
pub(super) fn annotation(lines: u64) -> String {
    format!("[distill: {lines} repeated lines omitted]\n")
}
