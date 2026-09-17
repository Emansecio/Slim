//! Adversarial coverage for `mcp/stdio.rs` `JsonLineFramer` (RODADA 2 —
//! Sifter). The framer is a public type; the size bound lives in the caller
//! (`spawn_reader` checks `pending_bytes` against `MAX_MESSAGE_BYTES` before
//! each push), so these tests pin the framer's own contract: newline framing,
//! noise conversion, and Unicode-safe truncation.

use serde_json::{json, Value};
use slim_core::mcp::{FramedLine, JsonLineFramer};

fn messages(lines: &[FramedLine]) -> Vec<&Value> {
    lines
        .iter()
        .filter_map(|line| match line {
            FramedLine::Message(value) => Some(value),
            FramedLine::Noise(_) => None,
        })
        .collect()
}

fn noises(lines: &[FramedLine]) -> Vec<&str> {
    lines
        .iter()
        .filter_map(|line| match line {
            FramedLine::Noise(text) => Some(text.as_str()),
            FramedLine::Message(_) => None,
        })
        .collect()
}

#[test]
fn fragmented_delivery_reassembles_messages_across_pushes() {
    let mut framer = JsonLineFramer::default();
    let payload = r#"{"jsonrpc":"2.0","method":"note","params":{"emoji":"🚀"}}"#;
    assert!(framer.push(&payload.as_bytes()[..10]).is_empty());
    assert!(framer.push(&payload.as_bytes()[10..30]).is_empty());
    // Split inside the emoji's four-byte UTF-8 sequence.
    let emoji_at = payload.find('🚀').expect("emoji");
    let split = emoji_at + 2;
    assert!(framer.push(&payload.as_bytes()[30..split]).is_empty());
    let lines = framer.push(&payload.as_bytes()[split..]);
    assert!(lines.is_empty(), "no newline yet: nothing may be emitted");
    let lines = framer.push(b"\n");
    assert_eq!(
        messages(&lines),
        vec![&json!({
            "jsonrpc": "2.0",
            "method": "note",
            "params": {"emoji": "🚀"},
        })]
    );
    assert_eq!(framer.pending_bytes(), 0);
}

#[test]
fn empty_and_whitespace_lines_are_not_messages() {
    let mut framer = JsonLineFramer::default();
    let lines = framer.push(b"\n\n\n");
    assert!(lines.is_empty());
    // Whitespace-only line is not empty: it becomes noise.
    let lines = framer.push(b"   \n");
    assert_eq!(noises(&lines), vec!["   "]);
    assert_eq!(framer.pending_bytes(), 0);
}

#[test]
fn noise_is_truncated_at_160_chars_with_ellipsis() {
    let mut framer = JsonLineFramer::default();
    let exact = "x".repeat(160);
    let lines = framer.push(format!("{exact}\n").as_bytes());
    assert_eq!(noises(&lines), vec![exact.as_str()]);

    let over = "y".repeat(161);
    let lines = framer.push(format!("{over}\n").as_bytes());
    let noise = noises(&lines);
    let noise = noise[0];
    assert_eq!(noise.chars().count(), 161);
    assert!(noise.ends_with('…'));
    assert_eq!(&noise[..160], &"y".repeat(160));
}

#[test]
fn noise_truncation_counts_chars_not_bytes() {
    let mut framer = JsonLineFramer::default();
    // 200 astral characters = 800 bytes; truncation must count chars.
    let line = "🚀".repeat(200);
    let lines = framer.push(format!("{line}\n").as_bytes());
    let noise = noises(&lines);
    let noise = noise[0];
    assert_eq!(noise.chars().count(), 161);
    assert!(noise.ends_with('…'));
    assert_eq!(&noise[..noise.len() - '…'.len_utf8()], &"🚀".repeat(160));
}

#[test]
fn noise_preserves_replacement_chars_for_invalid_utf8() {
    let mut framer = JsonLineFramer::default();
    let lines = framer.push(b"log \xFF\xFE broken\n");
    let noise = noises(&lines);
    assert_eq!(noise.len(), 1);
    assert!(noise[0].contains('\u{FFFD}'));
}

#[test]
fn crlf_terminated_json_parses_but_noise_keeps_the_cr() {
    let mut framer = JsonLineFramer::default();
    let lines = framer.push(b"{\"a\":1}\r\nsome log\r\n");
    let values = messages(&lines);
    assert_eq!(values, vec![&json!({"a": 1})]);
    // Noise is emitted with the trailing CR retained.
    assert_eq!(noises(&lines), vec!["some log\r"]);
}

#[test]
fn scalar_and_non_object_json_lines_become_messages() {
    let mut framer = JsonLineFramer::default();
    let lines = framer.push(b"5\nnull\n\"text\"\n[1,2]\n");
    let values = messages(&lines);
    assert_eq!(values.len(), 4);
    assert_eq!(values[0], &json!(5));
    assert_eq!(values[1], &Value::Null);
    assert_eq!(values[2], &json!("text"));
    assert_eq!(values[3], &json!([1, 2]));
}

#[test]
fn unterminated_tail_stays_pending_until_newline() {
    let mut framer = JsonLineFramer::default();
    assert!(framer.push(b"{\"a\":1").is_empty());
    assert_eq!(framer.pending_bytes(), 6);
    assert!(framer.push(b"}").is_empty());
    assert_eq!(framer.pending_bytes(), 7);
    let lines = framer.push(b"\n");
    assert_eq!(messages(&lines), vec![&json!({"a": 1})]);
    assert_eq!(framer.pending_bytes(), 0);
}

#[test]
fn consumed_lines_leave_the_partial_tail_buffered() {
    let mut framer = JsonLineFramer::default();
    let lines = framer.push(b"{\"a\":1}\n{\"b\"");
    assert_eq!(messages(&lines), vec![&json!({"a": 1})]);
    assert_eq!(framer.pending_bytes(), 4);
    let lines = framer.push(b":2}\n");
    assert_eq!(messages(&lines), vec![&json!({"b": 2})]);
}

#[test]
fn multiple_messages_arrive_in_one_chunk() {
    let mut framer = JsonLineFramer::default();
    let lines = framer.push(b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n");
    assert_eq!(
        messages(&lines),
        vec![&json!({"a": 1}), &json!({"b": 2}), &json!({"c": 3})]
    );
}

/// Documents the framer's contract: it buffers without a size cap; the
/// `spawn_reader` loop enforces `MAX_MESSAGE_BYTES` via `pending_bytes`
/// before pushing. Direct callers must enforce their own bound.
#[test]
fn framer_buffers_without_internal_size_cap() {
    let mut framer = JsonLineFramer::default();
    let big = vec![b'x'; 20 * 1024 * 1024];
    assert!(framer.push(&big).is_empty());
    assert_eq!(framer.pending_bytes(), big.len());
    // Terminating the line converts the whole thing into one noise entry,
    // truncated for display.
    let lines = framer.push(b"\n");
    let noise = noises(&lines);
    assert_eq!(noise.len(), 1);
    assert!(noise[0].ends_with('…'));
}

#[test]
fn utf8_bom_line_becomes_noise_not_a_message() {
    let mut framer = JsonLineFramer::default();
    // A BOM-prefixed JSON document is not valid JSON for serde_json.
    let lines = framer.push(b"\xEF\xBB\xBF{\"a\":1}\n");
    let noise = noises(&lines);
    assert_eq!(noise.len(), 1);
    assert!(noise[0].contains('\u{FEFF}'));
    assert!(messages(&lines).is_empty());
}
