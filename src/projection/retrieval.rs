use crate::types::ByteSpan;

/// A validated selector, resolved to concrete bounds by request policy. The
/// pattern is inert literal text: no pattern language reaches this module.
#[derive(Clone, Debug)]
pub(crate) enum Selection {
    Lines {
        start_line: u64,
        line_count: u64,
    },
    Pattern {
        pattern: String,
        before_lines: u64,
        after_lines: u64,
        max_matches: u64,
    },
}

/// Resolves a selection into sorted, disjoint, line-aligned source regions.
///
/// Line ranges and literal text patterns have no meaning over arbitrary bytes,
/// so the caller supplies text. Unselected projection keeps its existing binary
/// handling.
pub(crate) fn select_regions(text: &str, selection: &Selection) -> Vec<ByteSpan> {
    match selection {
        Selection::Lines {
            start_line,
            line_count,
        } => line_region(text, *start_line, *line_count),
        Selection::Pattern {
            pattern,
            before_lines,
            after_lines,
            max_matches,
        } => pattern_regions(text, pattern, *before_lines, *after_lines, *max_matches),
    }
}

fn line_region(text: &str, start_line: u64, line_count: u64) -> Vec<ByteSpan> {
    let start = nth_line_start(text, start_line.saturating_sub(1));
    if start >= text.len() {
        return Vec::new();
    }
    let end = nth_line_start(
        text,
        start_line.saturating_sub(1).saturating_add(line_count),
    );
    vec![ByteSpan {
        start: start as u64,
        end: end as u64,
    }]
}

fn pattern_regions(
    text: &str,
    pattern: &str,
    before_lines: u64,
    after_lines: u64,
    max_matches: u64,
) -> Vec<ByteSpan> {
    let mut regions: Vec<ByteSpan> = Vec::new();
    // `match_indices` runs the two-way substring search, so one pass over the
    // artifact bounds the scan regardless of the pattern length.
    for (offset, matched) in text.match_indices(pattern).take(max_matches as usize) {
        let mut start = line_start(text, offset);
        for _ in 0..before_lines {
            start = super::previous_line_start(text, start);
        }
        let mut end = line_end(text, offset + matched.len());
        for _ in 0..after_lines {
            if end >= text.len() {
                break;
            }
            end = super::next_line_start(text, end);
        }
        let region = ByteSpan {
            start: start as u64,
            end: end as u64,
        };
        // Matches arrive in increasing order, so a region can only extend or
        // follow the previous one.
        match regions.last_mut() {
            Some(last) if region.start <= last.end => last.end = last.end.max(region.end),
            _ => regions.push(region),
        }
    }
    regions
}

/// The byte offset of the start of the line containing `offset`.
fn line_start(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1)
}

/// The end of the line that `offset` closes: `offset` itself when it already
/// sits on a line boundary, and the end of the line containing it otherwise.
fn line_end(text: &str, offset: usize) -> usize {
    if offset == 0 || text.as_bytes().get(offset - 1) == Some(&b'\n') {
        return offset;
    }
    super::next_line_start(text, offset)
}

/// The byte offset where line `index` starts, or the text length when the text
/// has fewer lines.
fn nth_line_start(text: &str, index: u64) -> usize {
    let mut offset = 0;
    for _ in 0..index {
        if offset >= text.len() {
            return text.len();
        }
        offset = super::next_line_start(text, offset);
    }
    offset
}

/// Concatenates the selected regions into the buffer the planner projects.
pub(crate) fn render_regions(source: &[u8], regions: &[ByteSpan]) -> Vec<u8> {
    let mut selected = Vec::with_capacity(
        regions
            .iter()
            .map(|region| (region.end - region.start) as usize)
            .sum(),
    );
    for region in regions {
        selected.extend_from_slice(&source[region.start as usize..region.end as usize]);
    }
    selected
}

/// Translates spans expressed in selection-buffer offsets back into offsets in
/// the original committed source. A span that crosses a region boundary splits,
/// and spans that end up adjacent in the source merge again.
pub(crate) fn translate_spans(regions: &[ByteSpan], spans: &[ByteSpan]) -> Vec<ByteSpan> {
    let mut translated: Vec<ByteSpan> = Vec::new();
    for span in spans {
        let mut cursor = 0_u64;
        for region in regions {
            let length = region.end - region.start;
            let region_end = cursor + length;
            let overlap_start = span.start.max(cursor);
            let overlap_end = span.end.min(region_end);
            if overlap_start < overlap_end {
                let piece = ByteSpan {
                    start: region.start + (overlap_start - cursor),
                    end: region.start + (overlap_end - cursor),
                };
                match translated.last_mut() {
                    Some(last) if last.end == piece.start => last.end = piece.end,
                    _ => translated.push(piece),
                }
            }
            cursor = region_end;
        }
    }
    translated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(pairs: &[(u64, u64)]) -> Vec<ByteSpan> {
        pairs
            .iter()
            .map(|(start, end)| ByteSpan {
                start: *start,
                end: *end,
            })
            .collect()
    }

    const SOURCE: &str = "alpha\nbravo\ncharlie\ndelta\necho\n";

    #[test]
    fn line_ranges_are_clamped_to_the_source() {
        let select = |start_line, line_count| {
            select_regions(
                SOURCE,
                &Selection::Lines {
                    start_line,
                    line_count,
                },
            )
        };
        assert_eq!(select(2, 2), spans(&[(6, 20)]));
        assert_eq!(select(1, 99), spans(&[(0, 31)]));
        assert_eq!(select(5, 1), spans(&[(26, 31)]));
        assert!(select(6, 1).is_empty());
        assert!(select(99, 1).is_empty());

        // A source without a trailing newline still resolves its last line.
        assert_eq!(
            select_regions(
                "one\ntwo",
                &Selection::Lines {
                    start_line: 2,
                    line_count: 1
                }
            ),
            spans(&[(4, 7)])
        );
    }

    #[test]
    fn pattern_regions_expand_merge_and_stay_bounded() {
        let pattern = |pattern: &str, before, after, max_matches| {
            select_regions(
                SOURCE,
                &Selection::Pattern {
                    pattern: pattern.to_owned(),
                    before_lines: before,
                    after_lines: after,
                    max_matches,
                },
            )
        };

        assert_eq!(pattern("charlie", 0, 0, 8), spans(&[(12, 20)]));
        assert_eq!(pattern("charlie", 1, 1, 8), spans(&[(6, 26)]));
        // Context that reaches past either end clamps instead of overflowing.
        assert_eq!(pattern("charlie", 9, 9, 8), spans(&[(0, 31)]));
        // Distinct matches stay distinct; touching context merges them.
        assert_eq!(pattern("a\n", 0, 0, 8), spans(&[(0, 6), (20, 26)]));
        assert_eq!(pattern("a\n", 2, 2, 8), spans(&[(0, 31)]));
        // The match cap bounds the region count.
        assert_eq!(pattern("a\n", 0, 0, 1), spans(&[(0, 6)]));
        // An absent literal selects nothing and is not a failure.
        assert!(pattern("absent", 2, 2, 8).is_empty());
        // A multi-line pattern still expands to whole lines.
        assert_eq!(pattern("bravo\ncharlie", 0, 0, 8), spans(&[(6, 20)]));
    }

    #[test]
    fn patterns_are_literal_rather_than_a_pattern_language() {
        let literal = "a.c[0-9]+";
        let source = format!("head\nvalue {literal} tail\nfoot\n");
        let matched = select_regions(
            &source,
            &Selection::Pattern {
                pattern: literal.to_owned(),
                before_lines: 0,
                after_lines: 0,
                max_matches: 8,
            },
        );
        assert_eq!(matched, spans(&[(5, 26)]));

        // The same text read as a regular expression would match "abc1"; literal
        // search must not.
        assert!(
            select_regions(
                "head\nabc1\nfoot\n",
                &Selection::Pattern {
                    pattern: literal.to_owned(),
                    before_lines: 0,
                    after_lines: 0,
                    max_matches: 8,
                },
            )
            .is_empty()
        );
    }

    #[test]
    fn translation_splits_at_region_boundaries_and_rejoins_adjacent_pieces() {
        let regions = spans(&[(10, 20), (40, 50)]);
        // A span wholly inside the first region shifts by that region's start.
        assert_eq!(
            translate_spans(&regions, &spans(&[(2, 6)])),
            spans(&[(12, 16)])
        );
        // A span crossing the boundary splits into both source regions.
        assert_eq!(
            translate_spans(&regions, &spans(&[(5, 15)])),
            spans(&[(15, 20), (40, 45)])
        );
        // Pieces that meet in the source rejoin instead of staying separate.
        assert_eq!(
            translate_spans(&spans(&[(10, 20), (20, 30)]), &spans(&[(0, 20)])),
            spans(&[(10, 30)])
        );
        assert!(translate_spans(&regions, &[]).is_empty());
    }
}
