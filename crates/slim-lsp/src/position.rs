//! Position codec between "human" coordinates (1-based line, 1-based
//! character) and LSP coordinates (0-based line, 0-based character in the
//! position encoding negotiated with the server: UTF-8, UTF-16 or UTF-32.
//!
//! Never feed byte offsets of a Rust String directly into Position.character:
//! multi-byte characters, emoji (surrogate pairs), combining sequences and
//! CRLF line endings all shift the mapping. Every conversion in the crate
//! funnels through this codec.

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    /// LSP wire name, as sent in InitializeParams.capabilities.general.positionEncodings.
    pub fn lsp_name(&self) -> &'static str {
        match self {
            PositionEncoding::Utf8 => "utf-8",
            PositionEncoding::Utf16 => "utf-16",
            PositionEncoding::Utf32 => "utf-32",
        }
    }
}

impl fmt::Display for PositionEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.lsp_name())
    }
}

/// Parses a negotiated server encoding string. Returns UTF-16 (the LSP
/// historical default) for missing or unknown values, never an error:
/// spec-compliant clients must fall back to UTF-16.
pub fn encoding_from_lsp_name(name: Option<&str>) -> PositionEncoding {
    match name {
        Some("utf-8") => PositionEncoding::Utf8,
        Some("utf-32") => PositionEncoding::Utf32,
        _ => PositionEncoding::Utf16,
    }
}

/// Line/column conversions that are independent of file content ordering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PositionCodec;

impl PositionCodec {
    /// LSP character index for byte inside a line (byte must be on a char
    /// boundary). line is a single physical line without its newline terminator.
    pub fn byte_to_character(encoding: PositionEncoding, line: &str, byte: usize) -> Option<u32> {
        if byte > line.len() || !line.is_char_boundary(byte) {
            return None;
        }
        match encoding {
            PositionEncoding::Utf8 => u32::try_from(byte).ok(),
            PositionEncoding::Utf32 => u32::try_from(line[..byte].chars().count()).ok(),
            PositionEncoding::Utf16 => {
                u32::try_from(line[..byte].chars().map(|ch| ch.len_utf16()).sum::<usize>()).ok()
            }
        }
    }

    /// Byte offset inside line for a 0-based LSP character in encoding.
    /// Returns line.len() for a character equal to the line length (end of
    /// line), and None when the character is beyond the line.
    pub fn character_to_byte(
        encoding: PositionEncoding,
        line: &str,
        character: u32,
    ) -> Option<usize> {
        match encoding {
            PositionEncoding::Utf8 => {
                let target = character as usize;
                if target > line.len() {
                    return None;
                }
                let mut at = target;
                while at > 0 && !line.is_char_boundary(at) {
                    at -= 1;
                }
                Some(at)
            }
            PositionEncoding::Utf16 => {
                let mut units = 0u32;
                for (idx, ch) in line.char_indices() {
                    if units == character {
                        return Some(idx);
                    }
                    units = units.checked_add(ch.len_utf16() as u32)?;
                    if units > character {
                        // Character lands in the middle of an astral pair; the
                        // nearest previous boundary is the pair start.
                        return Some(idx);
                    }
                }
                if units == character {
                    Some(line.len())
                } else {
                    None
                }
            }
            PositionEncoding::Utf32 => {
                let count = line.chars().count();
                if character as usize > count {
                    return None;
                }
                if character as usize == count {
                    return Some(line.len());
                }
                Some(
                    line.char_indices()
                        .nth(character as usize)
                        .map(|(i, _)| i)
                        .unwrap_or(line.len()),
                )
            }
        }
    }

    /// Convert a human 1-based (line, column) into an LSP (line, character)
    /// for the given encoding using the actual text of that line. The human
    /// column counts Unicode scalar values (chars), not encoding units, so
    /// multi-byte characters map correctly in every negotiated encoding.
    pub fn human_to_lsp(
        encoding: PositionEncoding,
        physical_lines: &[String],
        human_line: u32,
        human_column: u32,
    ) -> Option<(u32, u32)> {
        if human_line == 0 {
            return None;
        }
        let line_index = human_line.checked_sub(1)? as usize;
        let text = physical_lines.get(line_index)?;
        let char_index = human_column.checked_sub(1)? as usize;
        let char_count = text.chars().count();
        // A column one past the last char addresses the end of the line
        // (editors do the same); anything further is invalid.
        if char_index > char_count {
            return None;
        }
        let byte = if char_index == char_count {
            text.len()
        } else {
            text.char_indices().nth(char_index)?.0
        };
        let lsp_column = Self::byte_to_character(encoding, text, byte)?;
        Some((line_index as u32, lsp_column))
    }

    /// Convert an LSP (line, character) into human 1-based (line, column),
    /// plus the byte offset of the character inside its line for later reads.
    /// The human column counts Unicode scalar values (chars), not encoding
    /// units.
    pub fn lsp_to_human(
        encoding: PositionEncoding,
        physical_lines: &[String],
        lsp_line: u32,
        lsp_character: u32,
    ) -> Option<(u32, u32, usize)> {
        let text = physical_lines.get(lsp_line as usize)?;
        let byte = Self::character_to_byte(encoding, text, lsp_character)?;
        let column = text.get(..byte)?.chars().count();
        Some((
            lsp_line.saturating_add(1),
            u32::try_from(column.saturating_add(1)).unwrap_or(u32::MAX),
            byte,
        ))
    }
}

/// Splits file content into physical lines. Both LF and CRLF are accepted;
/// the line terminator is stripped so position math never counts it.
pub fn split_physical_lines(content: &str) -> Vec<String> {
    content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(encoding: PositionEncoding, line: &str) {
        for (idx, _ch) in line.char_indices() {
            let character =
                PositionCodec::byte_to_character(encoding, line, idx).expect("boundary");
            let byte =
                PositionCodec::character_to_byte(encoding, line, character).expect("in range");
            assert_eq!(
                byte, idx,
                "roundtrip failed for {line:?} at char {character:?}"
            );
        }
        // End of line must also roundtrip.
        let eol_char = PositionCodec::byte_to_character(encoding, line, line.len()).expect("eol");
        assert_eq!(
            PositionCodec::character_to_byte(encoding, line, eol_char),
            Some(line.len())
        );
    }

    #[test]
    fn ascii_roundtrips_in_all_encodings() {
        for enc in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            roundtrip(enc, "fn execute_tool_call()");
            roundtrip(enc, "");
            roundtrip(enc, "a");
        }
    }

    #[test]
    fn accented_and_cjk_roundtrip_in_all_encodings() {
        for enc in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            roundtrip(enc, "\u{e1} \u{e9} \u{e7} \u{e3} \u{f5} \u{fc}");
            roundtrip(enc, "\u{6f22}\u{5b57}\u{306e}\u{30c6}\u{30b9}\u{30c8}");
            roundtrip(enc, "ma\u{e7}a caf\u{e9}");
        }
    }

    #[test]
    fn emoji_and_combining_roundtrip_in_all_encodings() {
        for enc in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            roundtrip(enc, "a\u{1F680}b"); // rocket: surrogate pair
            roundtrip(enc, "e\u{301}x"); // combining acute
            roundtrip(enc, "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"); // ZWJ family
            roundtrip(enc, "\u{10348}"); // astral non-emoji
        }
    }

    #[test]
    fn utf16_uses_code_units_not_code_points() {
        let line = "a\u{1F680}b";
        // a = 1 unit, rocket = 2 units, b = 1 unit.
        assert_eq!(
            PositionCodec::byte_to_character(PositionEncoding::Utf16, line, 0),
            Some(0)
        );
        assert_eq!(
            PositionCodec::byte_to_character(PositionEncoding::Utf16, line, 1),
            Some(1)
        );
        assert_eq!(
            PositionCodec::byte_to_character(PositionEncoding::Utf16, line, 5),
            Some(3)
        );
        assert_eq!(
            PositionCodec::character_to_byte(PositionEncoding::Utf16, line, 3),
            Some(5)
        );
        // Inside the pair is not a valid character position: maps to pair start.
        assert_eq!(
            PositionCodec::character_to_byte(PositionEncoding::Utf16, line, 2),
            Some(1)
        );
    }

    #[test]
    fn utf8_is_byte_based() {
        let line = "\u{6f22}\u{5b57}"; // 3 bytes each
        assert_eq!(
            PositionCodec::byte_to_character(PositionEncoding::Utf8, line, 0),
            Some(0)
        );
        assert_eq!(
            PositionCodec::byte_to_character(PositionEncoding::Utf8, line, 3),
            Some(3)
        );
        assert_eq!(
            PositionCodec::character_to_byte(PositionEncoding::Utf8, line, 6),
            Some(6)
        );
        // Mid-codepoint offset is invalid even in utf-8.
        assert_eq!(
            PositionCodec::character_to_byte(PositionEncoding::Utf8, line, 1),
            Some(0)
        );
        assert_eq!(
            PositionCodec::byte_to_character(PositionEncoding::Utf8, line, 1),
            None
        );
    }

    #[test]
    fn utf32_is_code_point_based() {
        let line = "a\u{1F680}b";
        assert_eq!(
            PositionCodec::byte_to_character(PositionEncoding::Utf32, line, 5),
            Some(2)
        );
        assert_eq!(
            PositionCodec::character_to_byte(PositionEncoding::Utf32, line, 2),
            Some(5)
        );
        assert_eq!(
            PositionCodec::character_to_byte(PositionEncoding::Utf32, line, 3),
            Some(6)
        );
    }

    #[test]
    fn human_coordinates_are_one_based_and_clamped_to_line_end() {
        let lines = split_physical_lines("first\nsecond line\r\nthird");
        let enc = PositionEncoding::Utf16;
        assert_eq!(PositionCodec::human_to_lsp(enc, &lines, 2, 1), Some((1, 0)));
        assert_eq!(PositionCodec::human_to_lsp(enc, &lines, 2, 7), Some((1, 6)));
        // Column 8 maps to 'l' of "line", which is LSP character 7.
        assert_eq!(PositionCodec::human_to_lsp(enc, &lines, 2, 8), Some((1, 7)));
        // Column 13 (beyond line end of 11-char "second line"): not a position.
        assert_eq!(PositionCodec::human_to_lsp(enc, &lines, 2, 13), None);
        // 0-based inputs are rejected.
        assert_eq!(PositionCodec::human_to_lsp(enc, &lines, 0, 1), None);
        // Line beyond file: rejected.
        assert_eq!(PositionCodec::human_to_lsp(enc, &lines, 4, 1), None);
        let (l, c, byte) = PositionCodec::lsp_to_human(enc, &lines, 1, 7).expect("position");
        assert_eq!((l, c), (2, 8));
        assert_eq!(&lines[1][byte..byte + 4], "line");
    }

    #[test]
    fn human_columns_count_chars_not_encoding_units() {
        // "a🚀b": scalars a, rocket, b; UTF-16 units 1, 2, 1.
        let lines = vec!["a\u{1F680}b".to_owned()];
        let utf16 = PositionEncoding::Utf16;
        assert_eq!(
            PositionCodec::human_to_lsp(utf16, &lines, 1, 1),
            Some((0, 0))
        );
        assert_eq!(
            PositionCodec::human_to_lsp(utf16, &lines, 1, 2),
            Some((0, 1))
        );
        assert_eq!(
            PositionCodec::human_to_lsp(utf16, &lines, 1, 3),
            Some((0, 3))
        );
        assert_eq!(
            PositionCodec::human_to_lsp(utf16, &lines, 1, 4),
            Some((0, 4))
        );
        assert_eq!(PositionCodec::human_to_lsp(utf16, &lines, 1, 5), None);
        assert_eq!(
            PositionCodec::lsp_to_human(utf16, &lines, 0, 1),
            Some((1, 2, 1))
        );
        assert_eq!(
            PositionCodec::lsp_to_human(utf16, &lines, 0, 3),
            Some((1, 3, 5))
        );
        // UTF-8: "éx" is 2 + 1 bytes; scalars é, x.
        let utf8_lines = vec!["\u{e9}x".to_owned()];
        let utf8 = PositionEncoding::Utf8;
        assert_eq!(
            PositionCodec::human_to_lsp(utf8, &utf8_lines, 1, 2),
            Some((0, 2))
        );
        assert_eq!(
            PositionCodec::lsp_to_human(utf8, &utf8_lines, 0, 2),
            Some((1, 2, 2))
        );
    }

    #[test]
    fn encoding_fallback_is_utf16() {
        assert_eq!(encoding_from_lsp_name(None), PositionEncoding::Utf16);
        assert_eq!(
            encoding_from_lsp_name(Some("utf-16")),
            PositionEncoding::Utf16
        );
        assert_eq!(
            encoding_from_lsp_name(Some("utf-8")),
            PositionEncoding::Utf8
        );
        assert_eq!(
            encoding_from_lsp_name(Some("weird")),
            PositionEncoding::Utf16
        );
    }
}
