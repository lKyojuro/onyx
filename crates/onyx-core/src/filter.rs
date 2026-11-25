//! Fast, Unicode-safe, case-insensitive inline fuzzy filtering and ranking for `FileEntry`.

use crate::sort::natural_cmp;
use crate::types::FileEntry;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

const SCORE_MATCH: i64 = 16;
const BONUS_CONSECUTIVE: i64 = 24;
const BONUS_START_OF_STRING: i64 = 32;
const BONUS_WORD_BOUNDARY: i64 = 24;
const BONUS_CAMEL_CASE: i64 = 20;
const BONUS_EXACT_CASE: i64 = 4;
const BONUS_EXACT_MATCH: i64 = 200;
const BONUS_EXACT_STEM_MATCH: i64 = 140;
const BONUS_PREFIX_MATCH: i64 = 80;

const PENALTY_GAP_START: i64 = 4;
const PENALTY_GAP_EXTEND: i64 = 1;
const PENALTY_LEADING_CHAR: i64 = 1;
const MAX_LEADING_PENALTY: i64 = 12;
const PENALTY_UNMATCHED_CHAR: i64 = 1;

/// Maximum `query_len * candidate_len` cell budget for exact dynamic programming.
/// Beyond this threshold, we fall back to linear greedy matching to keep latency bounded.
const MAX_DP_CELLS: usize = 16_384;

const NEG_INF: i64 = i64::MIN / 4;

/// Result of matching a fuzzy query against a single candidate string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuzzyMatch {
    /// Higher score indicates a stronger match.
    pub score: i64,
    /// Matched Unicode scalar value (`char`) indices in the original candidate string,
    /// strictly ascending and deduplicated.
    pub char_indices: Vec<usize>,
    /// Matched UTF-8 byte ranges `(start, end)` in the original candidate string,
    /// strictly ascending and non-overlapping.
    pub byte_ranges: Vec<(usize, usize)>,
}

impl FuzzyMatch {
    /// Returns an empty match representing an unfiltered item (empty query).
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            score: 0,
            char_indices: Vec::new(),
            byte_ranges: Vec::new(),
        }
    }
}

/// An owned `FileEntry` paired with its fuzzy match metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilteredEntry {
    pub entry: FileEntry,
    pub fuzzy_match: FuzzyMatch,
}

/// A borrowed `FileEntry` paired with its original slice index and fuzzy match metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredEntryRef<'a> {
    pub index: usize,
    pub entry: &'a FileEntry,
    pub fuzzy_match: FuzzyMatch,
}

#[derive(Debug, Clone, Copy)]
struct FoldedQueryChar {
    folded: char,
    orig: char,
}

#[derive(Debug, Clone, Copy)]
struct FoldedCandidateChar {
    folded: char,
    orig: char,
    char_idx: usize,
    byte_start: usize,
    byte_end: usize,
    boundary_bonus: i64,
}

#[inline]
fn is_word_separator(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '_' | '-' | '.' | '/' | '\\' | '(' | '[' | '{' | ':'
    )
}

fn fold_query(query: &str) -> Vec<FoldedQueryChar> {
    let mut out = Vec::with_capacity(query.len());
    for orig in query.chars() {
        for folded in orig.to_lowercase() {
            out.push(FoldedQueryChar { folded, orig });
        }
    }
    out
}

fn fold_candidate(candidate: &str) -> Vec<FoldedCandidateChar> {
    let mut out = Vec::with_capacity(candidate.len());
    let mut prev_orig: Option<char> = None;

    for (char_idx, (byte_start, orig)) in candidate.char_indices().enumerate() {
        let byte_end = byte_start + orig.len_utf8();
        let boundary_bonus = match prev_orig {
            None => BONUS_START_OF_STRING,
            Some(prev) if is_word_separator(prev) => BONUS_WORD_BOUNDARY,
            Some(prev) if prev.is_lowercase() && orig.is_uppercase() => BONUS_CAMEL_CASE,
            Some(_) => 0,
        };

        let mut first_folded = true;
        for folded in orig.to_lowercase() {
            out.push(FoldedCandidateChar {
                folded,
                orig,
                char_idx,
                byte_start,
                byte_end,
                boundary_bonus: if first_folded { boundary_bonus } else { 0 },
            });
            first_folded = false;
        }
        prev_orig = Some(orig);
    }

    out
}

/// Fast O(n + m) subsequence pre-check. Returns `true` if `query` is a subsequence of `candidate`.
fn is_subsequence(query: &[FoldedQueryChar], candidate: &[FoldedCandidateChar]) -> bool {
    let mut qi = 0usize;
    for c in candidate {
        if let Some(q) = query.get(qi)
            && c.folded == q.folded
        {
            qi += 1;
            if qi == query.len() {
                return true;
            }
        }
    }
    qi == query.len()
}

#[inline]
fn char_match_score(q: FoldedQueryChar, c: FoldedCandidateChar) -> i64 {
    let case_bonus = if q.orig == c.orig {
        BONUS_EXACT_CASE
    } else {
        0
    };
    SCORE_MATCH + c.boundary_bonus + case_bonus
}

fn whole_match_bonus(query: &[FoldedQueryChar], candidate: &[FoldedCandidateChar]) -> i64 {
    let mut bonus = 0i64;

    let is_prefix = candidate.len() >= query.len()
        && query
            .iter()
            .zip(candidate.iter())
            .all(|(q, c)| q.folded == c.folded);

    if is_prefix {
        bonus += BONUS_PREFIX_MATCH;
        if candidate.len() == query.len() {
            bonus += BONUS_EXACT_MATCH;
        } else if let Some(dot_pos) = candidate.iter().rposition(|c| c.folded == '.')
            && dot_pos == query.len()
        {
            bonus += BONUS_EXACT_STEM_MATCH;
        }
    }

    bonus
}

fn build_fuzzy_match(
    matched_folded_positions: &[usize],
    base_score: i64,
    query: &[FoldedQueryChar],
    candidate: &[FoldedCandidateChar],
) -> FuzzyMatch {
    let unmatched = candidate.len().saturating_sub(query.len()) as i64;
    let total_score =
        base_score + whole_match_bonus(query, candidate) - unmatched * PENALTY_UNMATCHED_CHAR;

    let mut char_indices = Vec::with_capacity(matched_folded_positions.len());
    let mut byte_ranges = Vec::with_capacity(matched_folded_positions.len());

    for &pos in matched_folded_positions {
        if let Some(c) = candidate.get(pos)
            && char_indices.last().copied() != Some(c.char_idx)
        {
            char_indices.push(c.char_idx);
            byte_ranges.push((c.byte_start, c.byte_end));
        }
    }

    FuzzyMatch {
        score: total_score,
        char_indices,
        byte_ranges,
    }
}

fn match_dp(query: &[FoldedQueryChar], candidate: &[FoldedCandidateChar]) -> Option<FuzzyMatch> {
    let m = query.len();
    let n = candidate.len();
    let cells = m.checked_mul(n)?;

    let mut dp = vec![NEG_INF; cells];
    let mut prev = vec![0usize; cells];

    // Row 0: match first query character at every matching candidate position.
    let q0 = *query.first()?;
    for (j, c) in candidate.iter().enumerate() {
        if c.folded == q0.folded {
            let leading_penalty = (j as i64).min(MAX_LEADING_PENALTY) * PENALTY_LEADING_CHAR;
            dp[j] = char_match_score(q0, *c) - leading_penalty;
        }
    }

    // Subsequent rows: O(1) transition per cell using running best_gap.
    for (i, &qi) in query.iter().enumerate().skip(1) {
        let row_offset = i * n;
        let prev_row_offset = (i - 1) * n;

        let mut best_gap = NEG_INF;
        let mut best_gap_k = 0usize;

        for j in i..n {
            if j > i {
                if best_gap > NEG_INF {
                    best_gap -= PENALTY_GAP_EXTEND;
                }
                let enter_k = j - 2;
                let enter_prev = dp[prev_row_offset + enter_k];
                if enter_prev > NEG_INF {
                    let enter_val = enter_prev - PENALTY_GAP_START;
                    if enter_val > best_gap {
                        best_gap = enter_val;
                        best_gap_k = enter_k;
                    }
                }
            }

            let cj = candidate[j];
            if cj.folded != qi.folded {
                continue;
            }

            let consec_prev = dp[prev_row_offset + (j - 1)];
            let consec = if consec_prev > NEG_INF {
                consec_prev + BONUS_CONSECUTIVE
            } else {
                NEG_INF
            };

            let (best_trans, best_k) = if consec >= best_gap {
                (consec, j - 1)
            } else {
                (best_gap, best_gap_k)
            };

            if best_trans > NEG_INF {
                dp[row_offset + j] = best_trans + char_match_score(qi, cj);
                prev[row_offset + j] = best_k;
            }
        }
    }

    let last_row = (m - 1) * n;
    let mut best_end_score = NEG_INF;
    let mut best_end_j = None;
    for j in (m - 1)..n {
        let s = dp[last_row + j];
        if s > best_end_score {
            best_end_score = s;
            best_end_j = Some(j);
        }
    }

    let mut cur_j = best_end_j?;
    let mut positions = vec![0usize; m];
    for i in (0..m).rev() {
        positions[i] = cur_j;
        if i > 0 {
            cur_j = prev[i * n + cur_j];
        }
    }

    Some(build_fuzzy_match(
        &positions,
        best_end_score,
        query,
        candidate,
    ))
}

fn match_greedy(
    query: &[FoldedQueryChar],
    candidate: &[FoldedCandidateChar],
) -> Option<FuzzyMatch> {
    let mut positions = Vec::with_capacity(query.len());
    let mut score = 0i64;
    let mut qi = 0usize;
    let mut prev_pos: Option<usize> = None;

    for (j, c) in candidate.iter().enumerate() {
        let Some(&q) = query.get(qi) else {
            break;
        };
        if c.folded == q.folded {
            score += char_match_score(q, *c);
            match prev_pos {
                None => {
                    score -= (j as i64).min(MAX_LEADING_PENALTY) * PENALTY_LEADING_CHAR;
                }
                Some(p) if p + 1 == j => {
                    score += BONUS_CONSECUTIVE;
                }
                Some(p) => {
                    let gap = (j - p - 1) as i64;
                    score -= PENALTY_GAP_START + (gap - 1) * PENALTY_GAP_EXTEND;
                }
            }
            positions.push(j);
            prev_pos = Some(j);
            qi += 1;
        }
    }

    if qi != query.len() {
        return None;
    }

    Some(build_fuzzy_match(&positions, score, query, candidate))
}

/// Performs case-insensitive fuzzy matching of `query` against `candidate`.
///
/// - An empty `query` always matches with score `0` and empty highlight indices.
/// - Returns `None` if `query` is not a case-insensitive subsequence of `candidate`.
/// - Returns `Some(FuzzyMatch)` with a ranking score and both `char_indices` and
///   UTF-8 `byte_ranges` suitable for highlighting in any UI toolkit.
#[must_use]
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    if query.is_empty() {
        return Some(FuzzyMatch::empty());
    }

    let q_folded = fold_query(query);
    if q_folded.is_empty() {
        return Some(FuzzyMatch::empty());
    }

    let c_folded = fold_candidate(candidate);
    if c_folded.len() < q_folded.len() || !is_subsequence(&q_folded, &c_folded) {
        return None;
    }

    let use_dp = q_folded
        .len()
        .checked_mul(c_folded.len())
        .is_some_and(|cells| cells <= MAX_DP_CELLS);

    if use_dp {
        match_dp(&q_folded, &c_folded)
    } else {
        match_greedy(&q_folded, &c_folded)
    }
}

/// Filters and ranks `entries` against `query`, returning borrowed references with their
/// original slice indices and highlight metadata.
///
/// - When `query` is empty, all entries are returned in their original order with empty matches.
/// - Otherwise, matching entries are sorted by descending fuzzy score, breaking ties
///   deterministically with `natural_cmp` on filename.
#[must_use]
pub fn filter_entry_refs<'a>(entries: &'a [FileEntry], query: &str) -> Vec<FilteredEntryRef<'a>> {
    if query.is_empty() {
        return entries
            .iter()
            .enumerate()
            .map(|(index, entry)| FilteredEntryRef {
                index,
                entry,
                fuzzy_match: FuzzyMatch::empty(),
            })
            .collect();
    }

    let q_folded = fold_query(query);
    if q_folded.is_empty() {
        return entries
            .iter()
            .enumerate()
            .map(|(index, entry)| FilteredEntryRef {
                index,
                entry,
                fuzzy_match: FuzzyMatch::empty(),
            })
            .collect();
    }

    let mut matched: Vec<FilteredEntryRef<'a>> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let c_folded = fold_candidate(&entry.name);
            if c_folded.len() < q_folded.len() || !is_subsequence(&q_folded, &c_folded) {
                return None;
            }
            let use_dp = q_folded
                .len()
                .checked_mul(c_folded.len())
                .is_some_and(|cells| cells <= MAX_DP_CELLS);
            let fuzzy_match = if use_dp {
                match_dp(&q_folded, &c_folded)?
            } else {
                match_greedy(&q_folded, &c_folded)?
            };
            Some(FilteredEntryRef {
                index,
                entry,
                fuzzy_match,
            })
        })
        .collect();

    matched.sort_by(|a, b| {
        b.fuzzy_match
            .score
            .cmp(&a.fuzzy_match.score)
            .then_with(|| natural_cmp(&a.entry.name, &b.entry.name))
            .then_with(|| a.index.cmp(&b.index))
    });

    matched
}

/// Filters and ranks `entries` against `query`, returning owned `FilteredEntry` values.
#[must_use]
pub fn filter_entries(entries: &[FileEntry], query: &str) -> Vec<FilteredEntry> {
    filter_entry_refs(entries, query)
        .into_iter()
        .map(|item| FilteredEntry {
            entry: item.entry.clone(),
            fuzzy_match: item.fuzzy_match,
        })
        .collect()
}

/// Compares two `FuzzyMatch` scores (descending) for custom ranking pipelines.
#[must_use]
pub fn cmp_fuzzy_score_desc(a: &FuzzyMatch, b: &FuzzyMatch) -> Ordering {
    b.score.cmp(&a.score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileAttributes, FileKind};
    use std::path::PathBuf;

    fn make_entry(name: &str) -> FileEntry {
        FileEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            extension: PathBuf::from(name)
                .extension()
                .map(|e| e.to_string_lossy().into_owned()),
            kind: FileKind::File,
            size: 1024,
            modified: Some(1_700_000_000),
            created: Some(1_700_000_000),
            attributes: FileAttributes::default(),
            is_symlink: false,
            symlink_target: None,
            type_label: String::new(),
        }
    }

    #[test]
    fn empty_query_matches_everything_in_original_order() {
        let entries = vec![make_entry("zeta.txt"), make_entry("alpha.rs")];
        let results = filter_entries(&entries, "");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].entry.name, "zeta.txt");
        assert_eq!(results[1].entry.name, "alpha.rs");
        assert!(results[0].fuzzy_match.char_indices.is_empty());
        assert!(results[0].fuzzy_match.byte_ranges.is_empty());
        assert_eq!(results[0].fuzzy_match.score, 0);
    }

    #[test]
    fn exact_and_prefix_and_consecutive_ranking() {
        let entries = vec![
            make_entry("my_application_index.rs"),
            make_entry("app_main.rs"),
            make_entry("main_window.rs"),
            make_entry("main.rs"),
            make_entry("main"),
            make_entry("unrelated.txt"),
        ];

        let ranked = filter_entries(&entries, "main");
        let names: Vec<&str> = ranked.iter().map(|r| r.entry.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "main",
                "main.rs",
                "main_window.rs",
                "app_main.rs",
                "my_application_index.rs",
            ]
        );
    }

    #[test]
    fn dp_prefers_consecutive_word_match_over_greedy_split() {
        let Some(m) = fuzzy_match("file", "config_file.toml") else {
            panic!("expected fuzzy match");
        };
        assert_eq!(m.char_indices, vec![7, 8, 9, 10]);
        assert_eq!(m.byte_ranges, vec![(7, 8), (8, 9), (9, 10), (10, 11)]);
    }

    #[test]
    fn camel_case_boundary_scores_higher_than_midword() {
        let Some(camel) = fuzzy_match("fe", "FileEntry.rs") else {
            panic!("expected camel match");
        };
        let Some(mid) = fuzzy_match("fe", "coffee_table.rs") else {
            panic!("expected midword match");
        };
        assert!(camel.score > mid.score);
        assert_eq!(camel.char_indices, vec![0, 4]);
    }

    #[test]
    fn case_insensitive_matching_and_highlight_indices() {
        let Some(m) = fuzzy_match("README", "readme.md") else {
            panic!("expected case-insensitive match");
        };
        assert_eq!(m.char_indices, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(
            m.byte_ranges,
            vec![(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (5, 6)]
        );
    }

    #[test]
    fn unicode_multibyte_and_case_folding_indices_are_valid_utf8() {
        let candidate = "🚀_Übersicht_日本語.txt";
        let Some(m) = fuzzy_match("üb日本", candidate) else {
            panic!("expected unicode match");
        };
        // '🚀' = char 0 (4 bytes), '_' = char 1 (1 byte), 'Ü' = char 2 (2 bytes), 'b' = char 3 (1 byte)
        assert_eq!(m.char_indices.len(), 4);
        assert_eq!(m.char_indices[0], 2);
        assert_eq!(m.char_indices[1], 3);

        // Verify every byte range slices valid UTF-8 and matches the expected chars.
        let extracted: Vec<&str> = m
            .byte_ranges
            .iter()
            .map(|&(start, end)| &candidate[start..end])
            .collect();
        assert_eq!(extracted, vec!["Ü", "b", "日", "本"]);
    }

    #[test]
    fn unicode_expanding_lowercase_deduplicates_indices() {
        // 'İ' (U+0130) lowercases to "i\u{307}" (2 chars).
        let candidate = "İstanbul.txt";
        let Some(m) = fuzzy_match("istanbul", candidate) else {
            panic!("expected Turkish I match");
        };
        assert_eq!(m.char_indices[0], 0);
        // Ensure strictly ascending with no duplicates.
        for pair in m.char_indices.windows(2) {
            assert!(pair[0] < pair[1]);
        }
        for &(start, end) in &m.byte_ranges {
            assert!(candidate.is_char_boundary(start));
            assert!(candidate.is_char_boundary(end));
            assert!(start < end);
        }
    }

    #[test]
    fn non_matching_and_query_longer_than_candidate_return_none() {
        assert!(fuzzy_match("xyz", "abc.txt").is_none());
        assert!(fuzzy_match("abcdef", "abc").is_none());
        assert!(fuzzy_match("a", "").is_none());
    }

    #[test]
    fn greedy_fallback_handles_large_inputs_safely() {
        let candidate = "a".repeat(500) + "needle" + &"b".repeat(500);
        let query = "a".repeat(20) + "needle";
        let Some(m) = fuzzy_match(&query, &candidate) else {
            panic!("expected greedy fallback match");
        };
        assert_eq!(m.char_indices.len(), query.len());
    }

    #[test]
    fn filter_entry_refs_preserves_original_indices() {
        let entries = vec![
            make_entry("alpha.txt"),
            make_entry("beta.rs"),
            make_entry("gamma_beta.rs"),
        ];
        let refs = filter_entry_refs(&entries, "beta");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].index, 1);
        assert_eq!(refs[0].entry.name, "beta.rs");
        assert_eq!(refs[1].index, 2);
        assert_eq!(refs[1].entry.name, "gamma_beta.rs");
    }
}
