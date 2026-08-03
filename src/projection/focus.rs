//! Intent-conditioned candidate ordering.
//!
//! A projector that cannot know why an observation is being read spends its
//! budget in source order. A caller that states what it is looking for lets the
//! same budget buy the lines that answer it instead.
//!
//! A focus is inert literal data. It is split into terms on everything that is
//! not a word character, and each term is compared as a lowercase substring:
//! there is no pattern language, no anchor, no escape, and no construct that can
//! name anything but text. It never enters the visible payload, the receipt, a
//! diagnostic, or a model instruction, and it can only reorder candidates the
//! shape policy already ranked or promote a line the source already contains.

/// The distinct terms one focus contributes. A question carries a handful of
/// content words; beyond this bound the extra terms only cost comparison work,
/// and the bound is what lets a match set be one machine word.
const MAX_TERMS: usize = 16;

/// The shortest term worth scoring. A single character occurs in nearly every
/// line of nearly every observation, so it ranks nothing.
const MIN_TERM_CHARS: usize = 2;

/// The share of an observation a term may occur in and still say where the
/// answer is. A word carried by more than a quarter of the lines describes the
/// observation rather than a region of it: it is the grammar of the question,
/// not its subject. Discarding it is what keeps a focus written as a sentence
/// from ranking by its articles and prepositions.
const MAX_TERM_LINE_SHARE: u64 = 4;

/// Which focus terms one line carries, as one bit per term.
pub(crate) type Match = u32;

/// The terms a request's focus scores candidate lines against.
#[derive(Clone, Debug, Default)]
pub(crate) struct Focus {
    terms: Vec<Box<str>>,
}

impl Focus {
    /// Compiles a validated focus into its distinct terms.
    ///
    /// Case folding is ASCII, which is the same folding the shape classifier
    /// applies to the lines this is compared against. A focus that yields no
    /// term scores nothing, which leaves selection exactly where it was.
    pub(crate) fn new(value: &str) -> Self {
        let mut terms: Vec<Box<str>> = Vec::new();
        for term in value.split(|character: char| !is_word_character(character)) {
            if terms.len() == MAX_TERMS {
                break;
            }
            if term.chars().count() < MIN_TERM_CHARS {
                continue;
            }
            let term = term.to_ascii_lowercase();
            if !terms.iter().any(|known| **known == *term) {
                terms.push(term.into_boxed_str());
            }
        }
        Self { terms }
    }

    /// The focus terms an already ASCII-lowercased line carries.
    pub(crate) fn matches(&self, lowercase_line: &str) -> Match {
        self.terms
            .iter()
            .enumerate()
            .filter(|(_, term)| lowercase_line.contains(&***term))
            .fold(0, |matched, (index, _)| matched | (1 << index))
    }

    /// The terms that discriminate: those the observation carries somewhere, but
    /// not nearly everywhere. Selection scores against these alone, so the rank
    /// of a line reflects what the caller is looking for rather than how the
    /// question happens to be phrased.
    pub(crate) fn discriminating(&self, matched_lines: &Counts, lines: u64) -> Match {
        (0..self.terms.len())
            .filter(|index| {
                let hits = matched_lines[*index];
                hits > 0 && hits * MAX_TERM_LINE_SHARE <= lines
            })
            .fold(0, |discriminating, index| discriminating | (1 << index))
    }
}

/// How many lines carried each focus term, by term index.
pub(crate) type Counts = [u64; MAX_TERMS];

/// Records one line's matches into the running per-term line counts. A line that
/// matched nothing, which is every line of an unfocused projection, costs
/// nothing.
pub(crate) fn accumulate(counts: &mut Counts, matched: Match) {
    if matched == 0 {
        return;
    }
    for (index, count) in counts.iter_mut().enumerate() {
        *count += u64::from(matched >> index & 1);
    }
}

/// How many of the discriminating terms a line carries.
pub(crate) fn score(matched: Match, discriminating: Match) -> u32 {
    (matched & discriminating).count_ones()
}

/// What a term is made of. Identifiers keep their underscores, so `focus_applied`
/// stays one term rather than two.
fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scores one line as if it were the only one an observation of `lines`
    /// lines had matched, which is what the term-rarity filter is measured on.
    fn score_alone(focus: &Focus, line: &str, lines: u64) -> u32 {
        let matched = focus.matches(line);
        let mut counts = Counts::default();
        accumulate(&mut counts, matched);
        score(matched, focus.discriminating(&counts, lines))
    }

    #[test]
    fn terms_are_bounded_distinct_and_case_folded() {
        let focus = Focus::new("Where is the RETRY budget, and where is retry?");
        assert_eq!(
            focus.terms.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            ["where", "is", "the", "retry", "budget", "and"]
        );
        assert_eq!(score_alone(&focus, "the retry budget is bounded", 100), 4);
        assert_eq!(score_alone(&focus, "nothing relevant here", 100), 0);

        let many = Focus::new(
            &(0..64)
                .map(|index| format!("term{index:03} "))
                .collect::<String>(),
        );
        assert_eq!(many.terms.len(), MAX_TERMS);
    }

    /// A focus is usually written as a sentence. The words that carry the
    /// question rather than its subject occur throughout the observation, so
    /// they rank nothing and the specific ones decide.
    #[test]
    fn a_term_carried_by_most_of_the_observation_ranks_nothing() {
        let focus = Focus::new("where is the retry budget consumed");
        let matched = focus.matches("the retry budget is consumed here");
        let common = focus.matches("the value is returned");

        let mut counts = Counts::default();
        accumulate(&mut counts, matched);
        for _ in 0..99 {
            accumulate(&mut counts, common);
        }
        let discriminating = focus.discriminating(&counts, 100);

        // "the" and "is" reach almost every line; "retry", "budget", and
        // "consumed" reach one.
        assert_eq!(score(matched, discriminating), 3);
        assert_eq!(score(common, discriminating), 0);
    }

    #[test]
    fn punctuation_is_a_separator_rather_than_an_operator() {
        // A pattern-language construct is split into ordinary terms instead of
        // being evaluated, so a focus can never describe more than literal text.
        // This expression matches "abcz" as a regular expression and scores
        // nothing, because every one of its terms is a separator or too short.
        assert_eq!(score_alone(&Focus::new("a.*z"), "abcz", 100), 0);
        assert_eq!(Focus::new("!!! ?").terms.len(), 0);
        // A shell construct is one ordinary term, matched as the text it is.
        assert_eq!(score_alone(&Focus::new("$(id)"), "the id column", 100), 1);

        let literal = Focus::new(r"^error\d+");
        assert_eq!(
            literal.terms.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            ["error"]
        );
        assert_eq!(
            score_alone(&literal, "error[e0308]: mismatched types", 100),
            1
        );
    }
}
