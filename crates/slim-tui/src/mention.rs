//! Fuzzy ranking for `@file` completion (DESIGN-SLIM-TUI §15.3).
//!
//! Pure functions over the host-supplied path list: no IO, no clock.

/// Upper bound on ranked candidates kept in state (§15.3).
pub const MAX_MENTION_MATCHES: usize = 500;

const BOUNDARY_BONUS: i32 = 8;
const CONSECUTIVE_BONUS: i32 = 6;
const FILE_NAME_BONUS: i32 = 24;
const FILE_NAME_PREFIX_BONUS: i32 = 16;
const PATH_PREFIX_BONUS: i32 = 10;

/// Indices into `files` ordered best-first for `query`, at most
/// `MAX_MENTION_MATCHES`. An empty query keeps the host order (shallower
/// first). Matching is a case-insensitive subsequence; ties keep host order.
pub fn rank(files: &[String], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..files.len().min(MAX_MENTION_MATCHES)).collect();
    }
    let mut scratch_haystack = Vec::new();
    let mut scored: Vec<(i32, usize)> = if query.is_ascii() {
        let needle_bytes: Vec<u8> = query
            .bytes()
            .map(|byte| {
                if byte == b'\\' {
                    b'/'
                } else {
                    byte.to_ascii_lowercase()
                }
            })
            .collect();
        let mut needle_chars: Option<Vec<char>> = None;
        files
            .iter()
            .enumerate()
            .filter_map(|(index, path)| {
                let score = if path.is_ascii() {
                    score_ascii(path.as_bytes(), &needle_bytes)
                } else {
                    let needle = needle_chars.get_or_insert_with(|| {
                        needle_bytes.iter().map(|&byte| char::from(byte)).collect()
                    });
                    score_with_scratch(path, needle, &mut scratch_haystack)
                };
                score.map(|score| (score, index))
            })
            .collect()
    } else {
        let needle: Vec<char> = query
            .chars()
            .map(|character| if character == '\\' { '/' } else { character })
            .flat_map(char::to_lowercase)
            .collect();
        files
            .iter()
            .enumerate()
            .filter_map(|(index, path)| {
                score_with_scratch(path, &needle, &mut scratch_haystack)
                    .map(|score| (score, index))
            })
            .collect()
    };
    // Stable sort: equal scores keep the host's shallower-first order.
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.truncate(MAX_MENTION_MATCHES);
    scored.into_iter().map(|(_, index)| index).collect()
}

fn score_ascii(haystack: &[u8], needle: &[u8]) -> Option<i32> {
    let name_start = haystack
        .iter()
        .rposition(|&byte| byte == b'/')
        .map_or(0, |index| index + 1);
    let in_path = subsequence_score_ascii(haystack, needle, 0).map(|score| {
        score
            + if starts_with_ascii_ci(haystack, needle) {
                PATH_PREFIX_BONUS
            } else {
                0
            }
    });
    let in_name = subsequence_score_ascii(haystack, needle, name_start).map(|score| {
        score
            + FILE_NAME_BONUS
            + if starts_with_ascii_ci(&haystack[name_start..], needle) {
                FILE_NAME_PREFIX_BONUS
            } else {
                0
            }
    });
    let best = match (in_path, in_name) {
        (Some(path), Some(name)) => path.max(name),
        (path, name) => path.or(name)?,
    };
    Some(best - i32::try_from(haystack.len() / 8).unwrap_or(0))
}

fn starts_with_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack[..needle.len()]
            .iter()
            .zip(needle)
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
}

fn subsequence_score_ascii(haystack: &[u8], needle: &[u8], from: usize) -> Option<i32> {
    let mut score = 0i32;
    let mut position = from;
    let mut previous_match: Option<usize> = None;
    for &wanted in needle {
        let found = haystack[position..]
            .iter()
            .position(|byte| byte.to_ascii_lowercase() == wanted)?
            + position;
        let at_boundary = found == 0
            || matches!(haystack[found - 1], b'/' | b'_' | b'-' | b'.' | b' ')
            || found == from;
        if at_boundary {
            score += BOUNDARY_BONUS;
        }
        match previous_match {
            Some(previous) if previous + 1 == found => score += CONSECUTIVE_BONUS,
            Some(previous) => {
                score -= i32::try_from((found - previous - 1).min(6)).unwrap_or(6);
            }
            None => {}
        }
        previous_match = Some(found);
        position = found + 1;
    }
    Some(score)
}

fn score_with_scratch(path: &str, needle: &[char], haystack: &mut Vec<char>) -> Option<i32> {
    haystack.clear();
    haystack.extend(path.chars().flat_map(char::to_lowercase));
    let name_start = haystack
        .iter()
        .rposition(|character| *character == '/')
        .map_or(0, |index| index + 1);
    let in_path = subsequence_score(haystack, needle, 0).map(|score| {
        score
            + if haystack.starts_with(needle) {
                PATH_PREFIX_BONUS
            } else {
                0
            }
    });
    let in_name = subsequence_score(haystack, needle, name_start).map(|score| {
        score
            + FILE_NAME_BONUS
            + if haystack[name_start..].starts_with(needle) {
                FILE_NAME_PREFIX_BONUS
            } else {
                0
            }
    });
    let best = match (in_path, in_name) {
        (Some(path), Some(name)) => path.max(name),
        (path, name) => path.or(name)?,
    };
    // Shorter paths win among comparable matches.
    Some(best - i32::try_from(haystack.len() / 8).unwrap_or(0))
}

/// Greedy left-to-right subsequence match starting at `from`, rewarding
/// segment starts and consecutive runs and charging for gaps.
fn subsequence_score(haystack: &[char], needle: &[char], from: usize) -> Option<i32> {
    let mut score = 0i32;
    let mut position = from;
    let mut previous_match: Option<usize> = None;
    for wanted in needle {
        let found = haystack[position..]
            .iter()
            .position(|character| character == wanted)?
            + position;
        let at_boundary = found == 0
            || matches!(haystack[found - 1], '/' | '_' | '-' | '.' | ' ')
            || found == from;
        if at_boundary {
            score += BOUNDARY_BONUS;
        }
        match previous_match {
            Some(previous) if previous + 1 == found => score += CONSECUTIVE_BONUS,
            Some(previous) => {
                score -= i32::try_from((found - previous - 1).min(6)).unwrap_or(6);
            }
            None => {}
        }
        previous_match = Some(found);
        position = found + 1;
    }
    Some(score)
}

/// Splits a workspace path into (directory including trailing `/`, file name).
pub fn split_path(path: &str) -> (&str, &str) {
    path.rfind('/')
        .map_or(("", path), |index| path.split_at(index + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    fn ranked<'a>(list: &'a [String], query: &str) -> Vec<&'a str> {
        rank(list, query)
            .into_iter()
            .map(|index| list[index].as_str())
            .collect()
    }

    #[test]
    fn empty_query_keeps_host_order() {
        let list = files(&["README.md", "src/lib.rs", "src/main.rs"]);
        assert_eq!(
            ranked(&list, ""),
            ["README.md", "src/lib.rs", "src/main.rs"]
        );
    }

    #[test]
    fn file_name_matches_beat_directory_matches() {
        let list = files(&["reducer/other.rs", "src/reducer.rs", "docs/red/er.txt"]);
        let hits = ranked(&list, "reducer");
        assert_eq!(hits[0], "src/reducer.rs");
        assert!(hits.contains(&"reducer/other.rs"));
    }

    #[test]
    fn subsequence_is_case_insensitive_and_skips_non_matches() {
        let list = files(&["src/Runtime.rs", "src/app.rs", "Cargo.toml"]);
        assert_eq!(ranked(&list, "rtme"), ["src/Runtime.rs"]);
        assert_eq!(ranked(&list, "CARGO"), ["Cargo.toml"]);
        assert!(ranked(&list, "zzz").is_empty());
    }

    #[test]
    fn backslash_query_matches_slash_paths() {
        let list = files(&["crates/slim-tui/src/app.rs", "app.rs"]);
        assert_eq!(ranked(&list, "tui\\src")[0], "crates/slim-tui/src/app.rs");
    }

    #[test]
    fn ties_keep_host_order_and_results_are_capped() {
        let list = files(&["a/x.rs", "b/x.rs"]);
        assert_eq!(ranked(&list, "x.rs"), ["a/x.rs", "b/x.rs"]);
        let many: Vec<String> = (0..MAX_MENTION_MATCHES + 50)
            .map(|index| format!("f{index}.rs"))
            .collect();
        assert_eq!(rank(&many, "f").len(), MAX_MENTION_MATCHES);
        assert_eq!(rank(&many, "").len(), MAX_MENTION_MATCHES);
    }

    #[test]
    fn split_path_separates_directory_and_name() {
        assert_eq!(split_path("src/a/b.rs"), ("src/a/", "b.rs"));
        assert_eq!(split_path("b.rs"), ("", "b.rs"));
    }

    #[test]
    fn ascii_and_unicode_paths_rank_consistently_across_case() {
        let list = files(&[
            "src/Ação.rs",
            "docs/ação/leia.md",
            "src/Runtime.rs",
            "crates/slim-tui/src/mention.rs",
        ]);
        assert_eq!(ranked(&list, "ação"), ["src/Ação.rs", "docs/ação/leia.md"]);
        assert_eq!(ranked(&list, "AÇÃO"), ["src/Ação.rs", "docs/ação/leia.md"]);
        assert_eq!(ranked(&list, "mention"), ["crates/slim-tui/src/mention.rs"]);
        assert_eq!(ranked(&list, "MENTION"), ["crates/slim-tui/src/mention.rs"]);
        assert_eq!(
            ranked(&list, "rtme"),
            ["src/Runtime.rs", "crates/slim-tui/src/mention.rs"]
        );
        assert_eq!(
            ranked(&list, "RTME"),
            ["src/Runtime.rs", "crates/slim-tui/src/mention.rs"]
        );
    }
}
