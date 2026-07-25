//! Fuzzy subsequence matching with ranking, shared by the launcher surfaces.
//!
//! Scoring is integer-only so results are deterministic and tests can assert
//! exact ordering. The algorithm mirrors fzf's simple matcher: a greedy forward
//! pass locates a match window, then a backward pass from the last matched index
//! tightens the start so `"ab"` prefers the later `a` in `"a-x-a-b"`.

const SCORE_BASE: i32 = 16;
const BONUS_FIRST_CHAR: i32 = 64;
const BONUS_AFTER_SEPARATOR: i32 = 48;
const BONUS_CAMEL_BOUNDARY: i32 = 32;
const BONUS_CONSECUTIVE: i32 = 24;
const BONUS_EXACT_CASE: i32 = 16;

const PENALTY_PER_GAP: i32 = 4;
const PENALTY_GAP_MAX: i32 = 40;
const PENALTY_LENGTH_MAX: i32 = 30;

const BONUS_EXACT: i32 = 400;
const BONUS_PREFIX: i32 = 200;
const BONUS_WORD_PREFIX: i32 = 120;

const SEPARATORS: [char; 5] = [' ', '-', '_', '.', '/'];

/// A successful match: its score and the haystack char indices that matched.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Match {
    pub score: i32,
    pub positions: Vec<usize>,
}

/// One searchable field with a relative weight, expressed in percent.
#[derive(Clone, Copy, Debug)]
pub struct Field<'a> {
    pub text: &'a str,
    pub weight: i32,
}

impl<'a> Field<'a> {
    pub fn new(text: &'a str, weight: i32) -> Self {
        Self { text, weight }
    }
}

/// Scores `needle` against `haystack`, returning `None` when it is not a subsequence.
///
/// An empty needle matches everything with a score of zero, which lets callers
/// use the same path for "show everything" and "filter".
pub fn match_query(needle: &str, haystack: &str) -> Option<Match> {
    let hay: Vec<char> = haystack.chars().collect();
    let hay_lower: Vec<char> = haystack.chars().flat_map(char::to_lowercase).collect();
    let needle_lower: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();

    if needle_lower.is_empty() {
        return Some(Match::default());
    }

    // `to_lowercase` can expand a char into several (e.g. 'İ'), which would
    // desynchronise the lowercase view from the original. Fall back to a
    // case-insensitive scan over the original chars when that happens.
    let hay_lower = if hay_lower.len() == hay.len() {
        hay_lower
    } else {
        hay.iter().map(|c| lower_first(*c)).collect()
    };

    let end = forward_scan(&hay_lower, &needle_lower)?;
    let start = backward_scan(&hay_lower, &needle_lower, end);
    let positions = collect_positions(&hay_lower, &needle_lower, start);
    let score = score_positions(&hay, &needle_lower, &positions);

    Some(Match {
        score: score + whole_string_bonus(needle, haystack),
        positions,
    })
}

/// Scores `needle` against several weighted fields, keeping the best result.
pub fn match_fields(needle: &str, fields: &[Field<'_>]) -> Option<Match> {
    let mut best: Option<Match> = None;

    for field in fields {
        let Some(mut found) = match_query(needle, field.text) else {
            continue;
        };
        found.score = found.score * field.weight / 100;

        if best
            .as_ref()
            .is_none_or(|current| found.score > current.score)
        {
            best = Some(found);
        }
    }

    best
}

/// Walks forward to find the earliest index at which the needle is complete.
fn forward_scan(hay: &[char], needle: &[char]) -> Option<usize> {
    let mut needle_index = 0;

    for (index, ch) in hay.iter().enumerate() {
        if *ch == needle[needle_index] {
            needle_index += 1;
            if needle_index == needle.len() {
                return Some(index);
            }
        }
    }

    None
}

/// Walks backward from `end` to find the latest possible start of the match.
fn backward_scan(hay: &[char], needle: &[char], end: usize) -> usize {
    let mut needle_index = needle.len();

    for index in (0..=end).rev() {
        if hay[index] == needle[needle_index - 1] {
            needle_index -= 1;
            if needle_index == 0 {
                return index;
            }
        }
    }

    0
}

/// Greedily matches the needle starting at `start`, recording each match index.
fn collect_positions(hay: &[char], needle: &[char], start: usize) -> Vec<usize> {
    let mut positions = Vec::with_capacity(needle.len());
    let mut needle_index = 0;

    for (index, ch) in hay.iter().enumerate().skip(start) {
        if needle_index == needle.len() {
            break;
        }
        if *ch == needle[needle_index] {
            positions.push(index);
            needle_index += 1;
        }
    }

    positions
}

fn score_positions(hay: &[char], needle: &[char], positions: &[usize]) -> i32 {
    let mut score = 0;
    let mut previous: Option<usize> = None;

    for (needle_index, position) in positions.iter().copied().enumerate() {
        score += SCORE_BASE;

        if position == 0 {
            score += BONUS_FIRST_CHAR;
        } else {
            let before = hay[position - 1];
            if SEPARATORS.contains(&before) {
                score += BONUS_AFTER_SEPARATOR;
            } else if before.is_lowercase() && hay[position].is_uppercase() {
                score += BONUS_CAMEL_BOUNDARY;
            }
        }

        if let Some(previous) = previous {
            if position == previous + 1 {
                score += BONUS_CONSECUTIVE;
            } else {
                let gap = (position - previous - 1) as i32;
                score -= (gap * PENALTY_PER_GAP).min(PENALTY_GAP_MAX);
            }
        }

        if hay[position] == needle[needle_index] {
            score += BONUS_EXACT_CASE;
        }

        previous = Some(position);
    }

    let extra = hay.len().saturating_sub(needle.len()) as i32;
    score - (extra / 4).min(PENALTY_LENGTH_MAX)
}

fn whole_string_bonus(needle: &str, haystack: &str) -> i32 {
    let needle = needle.to_lowercase();
    let haystack = haystack.to_lowercase();

    if haystack == needle {
        BONUS_EXACT
    } else if haystack.starts_with(&needle) {
        BONUS_PREFIX
    } else if haystack
        .split(SEPARATORS)
        .any(|word| !word.is_empty() && word.starts_with(&needle))
    {
        BONUS_WORD_PREFIX
    } else {
        0
    }
}

fn lower_first(value: char) -> char {
    value.to_lowercase().next().unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(needle: &str, haystack: &str) -> i32 {
        match_query(needle, haystack)
            .expect("expected a match")
            .score
    }

    #[test]
    fn empty_needle_matches_with_zero_score() {
        let found = match_query("", "Firefox").expect("empty needle matches");

        assert_eq!(found.score, 0);
        assert!(found.positions.is_empty());
    }

    #[test]
    fn rejects_non_subsequence() {
        assert!(match_query("xyz", "Firefox").is_none());
    }

    #[test]
    fn exact_beats_prefix_beats_word_prefix_beats_scattered() {
        let exact = score("firefox", "Firefox");
        let prefix = score("firefox", "Firefox Nightly");
        let word_prefix = score("firefox", "Mozilla Firefox Nightly");
        let scattered = score("firefox", "Fast Interactive Resource Explorer For OS X");

        assert!(exact > prefix, "{exact} > {prefix}");
        assert!(prefix > word_prefix, "{prefix} > {word_prefix}");
        assert!(word_prefix > scattered, "{word_prefix} > {scattered}");
    }

    #[test]
    fn prefers_the_shorter_more_relevant_haystack() {
        let focused = score("fox", "Firefox");
        let noisy = score("fox", "Fedora Media Writer Toolbox");

        assert!(focused > noisy, "{focused} > {noisy}");
    }

    #[test]
    fn rewards_consecutive_runs() {
        let consecutive = score("fire", "Firefox");
        let broken = score("fire", "F.i.r.e");

        assert!(consecutive > broken, "{consecutive} > {broken}");
    }

    #[test]
    fn rewards_word_boundaries() {
        let boundaries = score("vs", "Visual Studio Code");
        let interior = score("vs", "Advanced Settings");

        assert!(boundaries > interior, "{boundaries} > {interior}");
    }

    #[test]
    fn reports_matched_positions() {
        let found = match_query("re", "Firefox").expect("match");

        assert_eq!(found.positions, vec![2, 3]);
    }

    #[test]
    fn tightening_prefers_the_later_of_two_candidate_starts() {
        // "Firefox" has an f at 0 and at 4; the tighter window wins.
        let found = match_query("fx", "Firefox").expect("match");

        assert_eq!(found.positions, vec![4, 6]);
    }

    #[test]
    fn backward_pass_tightens_the_match_window() {
        let found = match_query("ab", "a-x-a-b").expect("match");

        assert_eq!(found.positions, vec![4, 6]);
    }

    #[test]
    fn weighted_fields_keep_the_best_score() {
        let fields = [Field::new("Files", 100), Field::new("filemanager", 50)];
        let found = match_fields("file", &fields).expect("match");

        assert_eq!(found.score, score("file", "Files"));
    }

    #[test]
    fn weighted_fields_skip_non_matching_entries() {
        let fields = [Field::new("Nothing", 100), Field::new("Firefox", 100)];

        assert!(match_fields("fox", &fields).is_some());
        assert!(match_fields("zzz", &fields).is_none());
    }
}
