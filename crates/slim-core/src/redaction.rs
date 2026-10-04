//! Heuristic credential redaction shared by the durable journal (CLI) and the
//! compaction fingerprint, which must hash the bytes the journal records.

use std::borrow::Cow;

pub(crate) const SENSITIVE_HEADER_NAMES: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "cookie",
    "set-cookie",
    "x-auth-token",
    "x-amz-security-token",
];

/// Streaming job output treats a credential field conservatively through end of line.
/// Unlike the JSON formatter this also recognizes an incomplete quoted key/value.
pub(crate) fn job_credential_value_start(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    SENSITIVE_HEADER_NAMES
        .iter()
        .flat_map(|name| {
            lower
                .match_indices(name)
                .map(move |(at, _)| (at, name.len()))
        })
        .filter_map(|(at, len)| {
            if at > 0
                && (lower.as_bytes()[at - 1].is_ascii_alphanumeric()
                    || lower.as_bytes()[at - 1] == b'-')
            {
                return None;
            }
            let mut end = at + len;
            if lower.as_bytes().get(end) == Some(&b'"') {
                end += 1;
            }
            while lower
                .as_bytes()
                .get(end)
                .is_some_and(u8::is_ascii_whitespace)
            {
                end += 1;
            }
            if lower.as_bytes().get(end) != Some(&b':') {
                return None;
            }
            end += 1;
            while lower
                .as_bytes()
                .get(end)
                .is_some_and(|b| b.is_ascii_whitespace() && *b != b'\n' && *b != b'\r')
            {
                end += 1;
            }
            Some(end)
        })
        .chain(job_json_keys(text).into_iter().filter_map(|(_, end, key)| {
            if !SENSITIVE_HEADER_NAMES
                .iter()
                .any(|name| key.eq_ignore_ascii_case(name))
            {
                return None;
            }
            let suffix = &text[end..];
            let whitespace = suffix.len() - suffix.trim_start().len();
            suffix[whitespace..]
                .strip_prefix(':')
                .map(|_| end + whitespace + 1)
        }))
        .min()
}

// Decode quoted names with serde so escaped JSON keys follow the same redaction contract.
fn job_json_keys(text: &str) -> Vec<(usize, usize, String)> {
    let mut keys = Vec::new();
    let mut from = 0;
    while let Some(relative) = text[from..].find('"') {
        let at = from + relative;
        let mut parser = serde_json::Deserializer::from_str(&text[at..]).into_iter::<String>();
        match parser.next() {
            Some(Ok(key)) => {
                from = at + parser.byte_offset();
                keys.push((at, from, key));
            }
            _ => from = at + 1,
        }
    }
    keys
}

pub(crate) fn job_pending_key_start(text: &str) -> Option<usize> {
    // Each ASCII character of a recognized name needs at most six bytes (\uXXXX).
    let max_key_bytes = SENSITIVE_HEADER_NAMES
        .iter()
        .map(|name| name.len())
        .max()
        .unwrap_or(0)
        * 6
        + 2;
    let mut from = 0;
    while let Some(relative) = text[from..].find('"') {
        let at = from + relative;
        let mut parser = serde_json::Deserializer::from_str(&text[at..]).into_iter::<String>();
        match parser.next() {
            Some(Ok(key)) => {
                from = at + parser.byte_offset();
                if text[from..].trim().is_empty()
                    && SENSITIVE_HEADER_NAMES
                        .iter()
                        .any(|name| key.eq_ignore_ascii_case(name))
                {
                    return Some(at);
                }
            }
            Some(Err(error)) if error.is_eof() && text.len() - at <= max_key_bytes => {
                return Some(at)
            }
            _ => from = at + 1,
        }
    }
    None
}

/// Redacts credential-bearing HTTP header values (`Authorization:`,
/// `Cookie:`, ...) and the same-named JSON fields. Input that parses as JSON is
/// re-serialized compactly; anything else is redacted line by line. Idempotent.
pub fn redact_credentials(input: &str) -> String {
    redact_credentials_cow(input).into_owned()
}

/// [`redact_credentials`] that borrows `input` when redaction leaves it
/// unchanged, which is the common case for text that is not JSON: only a text
/// that parses as JSON is always rebuilt (its compact form differs).
pub fn redact_credentials_cow(input: &str) -> Cow<'_, str> {
    if may_start_json(input) {
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(input) {
            redact_json_value(&mut value);
            if let Ok(redacted) = serde_json::to_string(&value) {
                return Cow::Owned(redacted);
            }
        }
    }
    if !may_hold_sensitive_header(input) {
        return Cow::Borrowed(input);
    }
    let mut redacted = String::with_capacity(input.len());
    for line in input.split_inclusive('\n') {
        let (content, newline) = if let Some(content) = line.strip_suffix("\r\n") {
            (content, "\r\n")
        } else {
            line.strip_suffix('\n')
                .map_or((line, ""), |content| (content, "\n"))
        };
        redacted.push_str(&redact_header_line(content));
        redacted.push_str(newline);
    }
    Cow::Owned(redacted)
}

/// Whether the first non-whitespace byte can start a JSON value; any other
/// text cannot parse as JSON, so the parse is skipped.
fn may_start_json(input: &str) -> bool {
    input
        .bytes()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| {
            matches!(byte, b'{' | b'[' | b'"' | b'-' | b't' | b'f' | b'n') || byte.is_ascii_digit()
        })
}

/// A cheap superset test for [`redact_header_line`]: some sensitive header
/// name ends right before optional whitespace and a colon. A text that fails
/// it is unchanged by the line pass.
fn may_hold_sensitive_header(input: &str) -> bool {
    let bytes = input.as_bytes();
    let mut from = 0;
    while let Some(offset) = bytes[from..].iter().position(|&byte| byte == b':') {
        let colon = from + offset;
        let name_end = bytes[..colon]
            .iter()
            .rposition(|byte| !byte.is_ascii_whitespace())
            .map_or(0, |index| index + 1);
        let before = &bytes[..name_end];
        if SENSITIVE_HEADER_NAMES.iter().any(|name| {
            before.len() >= name.len()
                && before[before.len() - name.len()..].eq_ignore_ascii_case(name.as_bytes())
        }) {
            return true;
        }
        from = colon + 1;
    }
    false
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (name, value) in object {
                if SENSITIVE_HEADER_NAMES
                    .iter()
                    .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
                {
                    *value = serde_json::Value::String("[REDACTED]".into());
                } else {
                    redact_json_value(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value);
            }
        }
        _ => {}
    }
}

fn redact_header_line(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut search_from = 0;
    let mut matches = Vec::new();
    while search_from < bytes.len() {
        let Some((index, header_len, value_start)) =
            find_sensitive_header(line, search_from, SENSITIVE_HEADER_NAMES)
        else {
            break;
        };
        matches.push((index, value_start));
        search_from = index + header_len;
    }
    if matches.is_empty() {
        return line.to_owned();
    }

    let mut result = String::with_capacity(line.len() + matches.len() * 4);
    let mut cursor = 0;
    for (index, (_, value_start)) in matches.iter().enumerate() {
        let end = matches
            .get(index + 1)
            .map_or(line.len(), |(next_index, _)| *next_index);
        result.push_str(&line[cursor..*value_start]);
        result.push_str("[REDACTED]");
        cursor = end;
    }
    result.push_str(&line[cursor..]);
    result
}

fn find_sensitive_header(
    line: &str,
    search_from: usize,
    header_names: &[&str],
) -> Option<(usize, usize, usize)> {
    let bytes = line.as_bytes();
    let mut best = None;
    for name in header_names {
        let Some(offset) = find_ascii_case_insensitive(&line[search_from..], name) else {
            continue;
        };
        let index = search_from + offset;
        let boundary_before =
            index == 0 || !bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b'-';
        if !boundary_before {
            continue;
        }
        let mut cursor = index + name.len();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b':' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if best.is_none_or(|(best_index, _, _)| index < best_index) {
            best = Some((index, name.len(), cursor));
        }
    }
    best
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Cow form must equal the owned form for every input: the fingerprint
    // hashes it and has to agree with the journal's redacted text.
    #[test]
    fn the_borrowing_form_matches_the_owned_form_and_borrows_plain_text() {
        let plain = [
            "",
            "plain text: with a colon\nand more lines\r\nend",
            "fn main() { let a: u32 = 1; }",
            "tomorrow I will go",
            "the cookie jar is full",
            "Cookie\nnot a header",
        ];
        for text in plain {
            assert_eq!(redact_credentials_cow(text), redact_credentials(text));
            assert!(
                matches!(redact_credentials_cow(text), Cow::Borrowed(_)),
                "{text:?}"
            );
        }
        let changed = [
            "run\nAuthorization: Bearer abc.def\nSet-Cookie: sid=1\nthen stop",
            "x-api-key :  k",
            "{\n  \"x-api-key\": \"k\",\n  \"path\": \"/v1\"\n}",
            "{\"b\": 1, \"a\": 2}",
            " 12.50",
            "[1, 2]",
            "{not json\nProxy-Authorization: x",
        ];
        for text in changed {
            let redacted = redact_credentials_cow(text);
            assert_eq!(redacted, redact_credentials(text));
            assert_ne!(redacted, text, "{text:?}");
            assert_eq!(redact_credentials(&redacted), redacted, "idempotent");
        }
    }
}
