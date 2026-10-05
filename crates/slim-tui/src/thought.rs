//! How a model's reasoning reads on screen: the one-row status of a thought
//! that is still being written, and the rows of its body.
//!
//! Reasoning arrives in two shapes. Summaries (OpenAI Responses) open each
//! part with a `**title**`; raw chains of thought (most other providers) are
//! plain prose. The status prefers the newest title, then the newest finished
//! sentence, so it changes in whole steps instead of crawling with every
//! token. Only a thought with nothing finished yet shows its newest words.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::markdown::{prose_row_count, render_prose, sanitize_terminal_text_cow};

/// What the header row of a streaming thought says after its clock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Status {
    /// The newest title, or the newest finished sentence: stable until the
    /// next one is complete.
    Headline(String),
    /// The newest words, cut from the left, while nothing is finished yet.
    Tail(String),
}

/// End of a thought read for its status, so a long thought costs the same
/// per update as a short one.
const WINDOW_BYTES: usize = 4_096;
/// A finished sentence shorter than this says too little to stand for the
/// thought ("Wait.", "Hmm, ok."), so an older one is tried instead.
const MIN_HEADLINE_WORDS: usize = 3;
/// Finished sentences tried, newest first, before falling back to the tail.
const HEADLINE_CANDIDATES: usize = 4;
/// Narrowest status worth showing.
const MIN_STATUS_CELLS: usize = 8;

/// Openers that carry no content, dropped from the front of a sentence when
/// punctuation follows them ("Okay,", "Certo.").
const FILLERS: &[&str] = &[
    "okay", "ok", "alright", "so", "hmm", "hmmm", "well", "wait", "right", "actually", "now",
    "great", "good", "perfect", "ah", "oh", "então", "bem", "certo", "beleza", "agora", "pronto",
    "perfeito", "ótimo", "espera",
];
/// Fillers dropped even without punctuation ("So the cache…").
const BARE_FILLERS: &[&str] = &["okay", "ok", "alright", "so", "hmm", "hmmm", "então"];

/// The status of a streaming thought in at most `cells` cells, or `None`
/// when there is nothing to show or no room for it.
pub(crate) fn status(text: &str, cells: usize) -> Option<Status> {
    if cells < MIN_STATUS_CELLS {
        return None;
    }
    let (window, truncated) = window(text);
    let window = sanitize_terminal_text_cow(window);
    if let Some(headline) = headline(&window, truncated) {
        return Some(Status::Headline(fit(&headline, cells)));
    }
    let tail = tail(&window, truncated, cells);
    (!tail.is_empty()).then_some(Status::Tail(tail))
}

/// Whether the status shows a headline rather than words still arriving.
pub(crate) fn has_headline(text: &str) -> bool {
    let (window, truncated) = window(text);
    headline(&sanitize_terminal_text_cow(window), truncated).is_some()
}

/// Body rows wrapped to `width`, each marked when it belongs to a title.
pub(crate) fn body_rows(text: &str, width: u16) -> Vec<(String, bool)> {
    let safe = sanitize_terminal_text_cow(text);
    display_lines(&safe)
        .flat_map(|(line, title)| {
            render_prose(&line, width)
                .into_iter()
                .map(move |row| (row, title))
        })
        .collect()
}

/// Rows of [`body_rows`] without building them.
pub(crate) fn body_row_count(text: &str, width: u16) -> usize {
    let safe = sanitize_terminal_text_cow(text);
    display_lines(&safe)
        .map(|(line, _)| prose_row_count(&line, width))
        .sum::<usize>()
        .max(1)
}

/// Source lines as shown: titles without their markers, emphasis markers
/// dropped from prose, fenced code untouched.
fn display_lines(safe: &str) -> impl Iterator<Item = (Cow<'_, str>, bool)> {
    let mut fenced = false;
    safe.split('\n').map(move |line| {
        if is_fence(line) {
            fenced = !fenced;
        } else if !fenced {
            if let Some(title) = title_text(line) {
                return (Cow::Owned(title.replace("**", "")), true);
            }
            if has_paired_emphasis(line) {
                return (Cow::Owned(line.replace("**", "")), false);
            }
        }
        (Cow::Borrowed(line), false)
    })
}

/// The newest title in `window`, else its newest finished sentence that says
/// enough.
fn headline(window: &str, truncated: bool) -> Option<String> {
    let mut fenced = false;
    let mut title = None;
    let mut sentences: Vec<&str> = Vec::with_capacity(HEADLINE_CANDIDATES);
    for (index, raw) in window.split_inclusive('\n').enumerate() {
        // A window that starts mid-thought starts mid-sentence.
        let partial = truncated && index == 0;
        let closed = raw.ends_with('\n');
        let line = raw.trim();
        if is_fence(line) {
            fenced = !fenced;
            continue;
        }
        if fenced || line.is_empty() {
            continue;
        }
        if let Some(text) = title_text(line).filter(|_| !partial) {
            title = Some(text);
            continue;
        }
        for sentence in finished_sentences(line, closed)
            .into_iter()
            .skip(usize::from(partial))
        {
            if sentences.len() == HEADLINE_CANDIDATES {
                sentences.remove(0);
            }
            sentences.push(sentence);
        }
    }
    if let Some(title) = title {
        return Some(title.replace("**", ""));
    }
    sentences
        .iter()
        .rev()
        .find_map(|sentence| clean_sentence(sentence))
}

/// Sentences of `line` that are complete: each one closed by `.`, `!`, `?` or
/// `…` before a space, and the remainder too when the line itself has ended.
fn finished_sentences(line: &str, closed: bool) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if matches!(character, '.' | '!' | '?' | '…')
            && chars.peek().is_some_and(|(_, next)| next.is_whitespace())
        {
            let end = index + character.len_utf8();
            sentences.push(&line[start..end]);
            start = end;
        }
    }
    if closed && start < line.len() {
        sentences.push(&line[start..]);
    }
    sentences
}

/// A sentence as a headline: list marker, emphasis and empty openers gone,
/// capitalized, without closing punctuation. `None` when what is left is too
/// short to say anything.
fn clean_sentence(sentence: &str) -> Option<String> {
    let text = strip_list_marker(sentence.trim()).replace("**", "");
    let mut words: Vec<&str> = text.split_whitespace().collect();
    while let Some(first) = words.first() {
        let bare = first.trim_end_matches([',', '.', '!', ':', ';', '…', '-', '—']);
        let lower = bare.to_lowercase();
        let punctuated = bare.len() < first.len();
        if FILLERS.contains(&lower.as_str())
            && (punctuated || BARE_FILLERS.contains(&lower.as_str()))
        {
            words.remove(0);
        } else {
            break;
        }
    }
    if words.len() < MIN_HEADLINE_WORDS {
        return None;
    }
    let joined = words.join(" ");
    let joined = joined.trim_end_matches(['.', ',', ':', ';']);
    let mut characters = joined.chars();
    let first = characters.next()?;
    Some(first.to_uppercase().chain(characters).collect())
}

fn strip_list_marker(line: &str) -> &str {
    for marker in ["- ", "* ", "+ ", "> "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    line
}

/// The text of a title line: `**Title**` or a Markdown heading.
fn title_text(line: &str) -> Option<&str> {
    let line = line.trim();
    if let Some(inner) = line
        .strip_prefix("**")
        .and_then(|rest| rest.strip_suffix("**"))
    {
        let inner = inner.trim();
        return (!inner.is_empty() && !inner.contains("**")).then_some(inner);
    }
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&hashes) {
        return line[hashes..]
            .strip_prefix(' ')
            .map(str::trim)
            .filter(|text| !text.is_empty());
    }
    None
}

fn is_fence(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("```") || line.starts_with("~~~")
}

/// Whether every `**` in `line` has a partner, so removing them cannot eat
/// a marker whose closing half has not arrived yet.
fn has_paired_emphasis(line: &str) -> bool {
    let count = line.matches("**").count();
    count > 0 && count.is_multiple_of(2)
}

/// `text` in at most `cells` cells, cut at a word with `…` when it does not
/// fit.
pub(crate) fn fit(text: &str, cells: usize) -> String {
    if UnicodeWidthStr::width(text) <= cells {
        return text.to_owned();
    }
    let budget = cells.saturating_sub(1);
    let mut used = 0;
    let mut end = 0;
    for (index, grapheme) in text.grapheme_indices(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width > budget {
            break;
        }
        used += width;
        end = index + grapheme.len();
    }
    let kept = &text[..end];
    // Back off to the last whole word unless that throws most of it away.
    let kept = match kept.rfind(char::is_whitespace) {
        Some(space) if space * 2 >= kept.len() => &kept[..space],
        _ => kept,
    };
    format!("{}…", kept.trim_end_matches([' ', ',', ';', ':', '.']))
}

/// The newest words in at most `cells` cells: whitespace folded, emphasis
/// markers dropped, cut from the left at a word with `…`.
fn tail(window: &str, truncated: bool, cells: usize) -> String {
    let folded = window
        .split_whitespace()
        .map(|word| word.trim_matches('*'))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !truncated && UnicodeWidthStr::width(folded.as_str()) <= cells {
        return folded;
    }
    let budget = cells.saturating_sub(1);
    let mut used = 0;
    let mut start = folded.len();
    for (index, grapheme) in folded.grapheme_indices(true).rev() {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width > budget {
            break;
        }
        used += width;
        start = index;
    }
    let mut kept = &folded[start..];
    // A cut inside a word drops that word's visible end too.
    if start > 0 && !folded[..start].ends_with(' ') {
        if let Some(space) = kept.find(' ') {
            kept = &kept[space + 1..];
        }
    }
    if kept.is_empty() {
        String::new()
    } else {
        format!("…{kept}")
    }
}

/// The last [`WINDOW_BYTES`] of `text`, and whether anything came before.
fn window(text: &str) -> (&str, bool) {
    let mut start = text.len().saturating_sub(WINDOW_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (&text[start..], start > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headline_of(text: &str) -> Option<String> {
        match status(text, 80)? {
            Status::Headline(headline) => Some(headline),
            Status::Tail(_) => None,
        }
    }

    #[test]
    fn summary_title_wins_while_its_paragraph_streams() {
        let text = "**Reading the render path**\n\nThe header is patched after the cache. Next the";
        assert_eq!(
            headline_of(text).as_deref(),
            Some("Reading the render path")
        );
        let text = format!("{text} glow.\n\n## Checking the cache\n\nIt keys on");
        assert_eq!(headline_of(&text).as_deref(), Some("Checking the cache"));
    }

    #[test]
    fn raw_reasoning_shows_its_newest_finished_sentence() {
        let text = "Okay, so the user wants a calmer header. Let me read the render";
        assert_eq!(
            headline_of(text).as_deref(),
            Some("The user wants a calmer header")
        );
        let text = "The cache keys on the generation. Hmm. Wait. Then";
        assert_eq!(
            headline_of(text).as_deref(),
            Some("The cache keys on the generation")
        );
        let text = "certo, então vou conferir o cache de linhas.\n- ler o render";
        assert_eq!(
            headline_of(text).as_deref(),
            Some("Vou conferir o cache de linhas")
        );
    }

    #[test]
    fn nothing_finished_shows_the_newest_words_cut_at_a_word() {
        assert_eq!(
            status("Let me look at", 40),
            Some(Status::Tail("Let me look at".into()))
        );
        assert_eq!(
            status("one two three four five six seven", 16),
            Some(Status::Tail("…five six seven".into()))
        );
        assert_eq!(status("", 40), None);
        assert_eq!(status("anything at all", 7), None);
    }

    #[test]
    fn fenced_code_never_becomes_a_headline() {
        let text = "Plan the change first.\n```sh\n# install deps\ncargo build.\n```\nThen";
        assert_eq!(headline_of(text).as_deref(), Some("Plan the change first"));
    }

    #[test]
    fn long_headlines_end_at_a_word() {
        let text = "**Reading every renderer in the workspace carefully**\n";
        assert_eq!(
            status(text, 24),
            Some(Status::Headline("Reading every renderer…".into()))
        );
    }

    #[test]
    fn body_drops_markers_and_marks_titles() {
        let text = "**Plan**\n\nUse **one** sweep.\n```\n# kept\n```";
        let rows = body_rows(text, 40);
        assert_eq!(
            rows,
            vec![
                ("Plan".to_owned(), true),
                (String::new(), false),
                ("Use one sweep.".to_owned(), false),
                ("```".to_owned(), false),
                ("# kept".to_owned(), false),
                ("```".to_owned(), false),
            ]
        );
        assert_eq!(body_row_count(text, 40), rows.len());
    }

    #[test]
    fn body_row_count_matches_rows_when_wrapping() {
        let text = "**A title long enough to wrap twice here**\nplain **bold** words that wrap\n\n";
        for width in [1, 5, 12, 80] {
            assert_eq!(body_row_count(text, width), body_rows(text, width).len());
        }
    }

    #[test]
    fn long_thoughts_read_only_their_end() {
        let text = format!("{}\n**Final title**\n", "word ".repeat(5_000));
        assert_eq!(headline_of(&text).as_deref(), Some("Final title"));
        assert!(has_headline(&text));
        // One line with no break at all still yields its newest sentence.
        let text = format!("{}Last one", "Each sentence is here. ".repeat(500));
        assert_eq!(headline_of(&text).as_deref(), Some("Each sentence is here"));
        assert!(!has_headline("still writing the first"));
    }
}
