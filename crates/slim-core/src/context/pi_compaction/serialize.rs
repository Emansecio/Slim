//! Conversation serialization for summarization requests.
//!
//! Port of Pi's `serializeConversation` and `truncateForSummary`
//! (`packages/coding-agent/src/core/compaction/utils.ts`), Copyright (c) 2025
//! Mario Zechner, MIT License (https://github.com/earendil-works/pi). The
//! transcript is rendered as labelled text so the summarizer reads it as data
//! to condense instead of a conversation to continue.
//!
//! Deviation from Pi: Pi renders tool-call arguments and thinking whole and
//! bounds only tool results. A `write` or `patch` argument carries a file, and
//! Slim's summary request is fitted to the context window (`bound_conversation`
//! drops the middle of the conversation when it does not fit), so every string
//! argument value and every thinking text is bounded here like a tool result,
//! with the same marker.

use crate::provider::{ProviderContentBlock, ProviderMessage, ProviderToolCall};
use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

/// Maximum characters of one tool result in a serialized summary request.
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Maximum characters of one string value of a tool call's arguments (at any
/// depth) in a serialized summary request.
const TOOL_ARGUMENT_MAX_CHARS: usize = 2000;

/// Maximum characters of one assistant thinking text in a serialized summary
/// request.
const THINKING_MAX_CHARS: usize = 2000;

/// Text of a message: its `content` followed by its text blocks, joined with
/// `separator` (Pi's `contentText`: `""` for user and tool results, `"\n"` for
/// assistant text). Images, audio, files and opaque reasoning carry no
/// readable text.
pub(super) fn message_text(message: &ProviderMessage, separator: &str) -> String {
    let mut text = message.content.clone();
    for block in &message.content_blocks {
        if let ProviderContentBlock::Text(block_text) = block {
            if !text.is_empty() {
                text.push_str(separator);
            }
            text.push_str(block_text);
        }
    }
    text
}

/// Keeps the beginning of `text` and appends Pi's truncation marker.
pub(super) fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_owned();
    }
    let kept = text
        .char_indices()
        .nth(max_chars)
        .map_or(text, |(end, _)| &text[..end]);
    format!(
        "{kept}\n\n[... {} more characters truncated]",
        total - max_chars
    )
}

/// A parsed JSON value whose objects keep the order of the source text (the
/// order of a JS object, which is what Pi stringifies). `serde_json::Value`
/// sorts keys.
enum OrderedValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<OrderedValue>),
    Object(Vec<(String, OrderedValue)>),
}

impl<'de> Deserialize<'de> for OrderedValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = OrderedValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedValue::Null)
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(OrderedValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(OrderedValue::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(OrderedValue::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
                Ok(serde_json::Number::from_f64(value)
                    .map_or(OrderedValue::Null, OrderedValue::Number))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(OrderedValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(OrderedValue::String(value))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(OrderedValue::Array(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry::<String, OrderedValue>()? {
                    entries.push(entry);
                }
                Ok(OrderedValue::Object(js_property_order(entries)))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}

/// `Some(index)` for a key JS treats as an array index: canonical decimal,
/// below 2^32 - 1.
fn array_index(key: &str) -> Option<u32> {
    let canonical = key == "0" || (!key.starts_with('0') && !key.is_empty());
    (canonical && key.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| key.parse::<u32>().ok().filter(|&index| index != u32::MAX))
        .flatten()
}

/// The entries in the order JS enumerates them: array-index keys ascending,
/// then the others in insertion order, a repeated key keeping its first
/// position with its last value.
fn js_property_order(entries: Vec<(String, OrderedValue)>) -> Vec<(String, OrderedValue)> {
    let mut position: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut merged: Vec<(String, OrderedValue)> = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        match position.get(&key) {
            Some(&index) => merged[index].1 = value,
            None => {
                position.insert(key.clone(), merged.len());
                merged.push((key, value));
            }
        }
    }
    let (mut indexed, others): (Vec<_>, Vec<_>) = merged
        .into_iter()
        .partition(|(key, _)| array_index(key).is_some());
    indexed.sort_by_key(|(key, _)| array_index(key));
    indexed.extend(others);
    indexed
}

/// A number the way `JSON.stringify` prints it: integral values without a
/// fraction, shortest round-trip digits, exponent form outside [1e-6, 1e21).
fn js_number(number: &serde_json::Number) -> String {
    const MAX_EXACT: u64 = 1 << 53;
    if let Some(value) = number.as_i64().filter(|v| v.unsigned_abs() <= MAX_EXACT) {
        return value.to_string();
    }
    if let Some(value) = number.as_u64().filter(|&v| v <= MAX_EXACT) {
        return value.to_string();
    }
    let value = number.as_f64().unwrap_or(0.0);
    if value == 0.0 {
        return "0".into();
    }
    let magnitude = value.abs();
    if (1e-6..1e21).contains(&magnitude) {
        return if value.fract() == 0.0 {
            format!("{value:.0}")
        } else {
            value.to_string()
        };
    }
    let text = format!("{value:e}");
    match text.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => {
            format!("{mantissa}e+{exponent}")
        }
        _ => text,
    }
}

/// Appends the compact JSON text of `value`, objects in source order.
fn write_json(value: &OrderedValue, out: &mut String) {
    match value {
        OrderedValue::Null => out.push_str("null"),
        OrderedValue::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        OrderedValue::Number(number) => out.push_str(&js_number(number)),
        OrderedValue::String(text) => {
            let text = truncate_for_summary(text, TOOL_ARGUMENT_MAX_CHARS);
            out.push_str(&serde_json::to_string(&text).unwrap_or_default());
        }
        OrderedValue::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        OrderedValue::Object(entries) => {
            out.push('{');
            for (index, (key, value)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                write_json(value, out);
            }
            out.push('}');
        }
    }
}

/// A tool call's arguments: a JSON object whose entries keep their source
/// order.
struct OrderedArguments(Vec<(String, OrderedValue)>);

impl<'de> Deserialize<'de> for OrderedArguments {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match OrderedValue::deserialize(deserializer)? {
            OrderedValue::Object(entries) => Ok(Self(entries)),
            _ => Err(serde::de::Error::custom("arguments are not a JSON object")),
        }
    }
}

/// `name(key=json, key=json)` as Pi renders a tool call, with `JSON.stringify`
/// text for every value. Arguments that are not a JSON object (empty,
/// malformed or another JSON type) are rendered as one `arguments=` entry
/// holding the raw text.
fn render_tool_call(call: &ProviderToolCall) -> String {
    let rendered = if call.arguments.trim().is_empty() {
        String::new()
    } else if let Ok(OrderedArguments(entries)) =
        serde_json::from_str::<OrderedArguments>(&call.arguments)
    {
        entries
            .iter()
            .map(|(key, value)| {
                let mut json = String::new();
                write_json(value, &mut json);
                format!("{key}={json}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        format!(
            "arguments={}",
            serde_json::Value::String(truncate_for_summary(
                &call.arguments,
                TOOL_ARGUMENT_MAX_CHARS
            ))
        )
    };
    format!("{}({rendered})", call.name)
}

/// Serializes messages to the `[User]: ...` / `[Assistant]: ...` text Pi sends
/// to the summarizer. Tool results, thinking texts and each string value of a
/// tool call's arguments are truncated to 2000 characters; user and assistant
/// text is kept whole. System and developer messages are not part of the
/// conversation and are skipped.
pub fn serialize_conversation<'a>(
    messages: impl IntoIterator<Item = &'a ProviderMessage>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for message in messages {
        match message.role.as_str() {
            "user" => {
                let text = message_text(message, "");
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            "assistant" => {
                if let Some(reasoning) = &message.chat_reasoning {
                    if !reasoning.content.is_empty() {
                        parts.push(format!(
                            "[Assistant thinking]: {}",
                            truncate_for_summary(&reasoning.content, THINKING_MAX_CHARS)
                        ));
                    }
                }
                let text = message_text(message, "\n");
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                if !message.tool_calls.is_empty() {
                    let calls = message
                        .tool_calls
                        .iter()
                        .map(render_tool_call)
                        .collect::<Vec<_>>()
                        .join("; ");
                    parts.push(format!("[Assistant tool calls]: {calls}"));
                }
            }
            "tool" => {
                let text = message_text(message, "");
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_result(content: String) -> ProviderMessage {
        ProviderMessage::tool("read", "tc1", content)
    }

    // Pi: compaction-serialization.test.ts
    #[test]
    fn truncates_long_tool_results() {
        let long = "x".repeat(5000);
        let result = serialize_conversation([&tool_result(long)]);

        assert!(result.contains("[Tool result]:"));
        assert!(result.contains("[... 3000 more characters truncated]"));
        assert!(!result.contains(&"x".repeat(3000)));
        assert!(result.contains(&"x".repeat(2000)));
    }

    #[test]
    fn keeps_short_tool_results_whole() {
        let short = "x".repeat(1500);
        let result = serialize_conversation([&tool_result(short.clone())]);

        assert_eq!(result, format!("[Tool result]: {short}"));
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn bounds_string_arguments_and_thinking_but_not_other_text() {
        let file = "w".repeat(5000);
        let call = ProviderToolCall {
            id: "c1".into(),
            name: "write".into(),
            arguments: serde_json::json!({
                "path": "a.rs",
                "content": file,
                "edits": [{"expected": "e".repeat(2500)}],
                "limit": 7,
            })
            .to_string(),
        };
        let raw = ProviderToolCall {
            id: "c2".into(),
            name: "patch".into(),
            arguments: format!(r#"{{"path": "{}"#, "m".repeat(3000)),
        };
        let mut assistant = ProviderMessage::assistant(file.clone(), vec![call, raw]);
        assistant.chat_reasoning = Some(crate::provider::ChatReasoning {
            scope_id: 1,
            model: "m".into(),
            content: "t".repeat(2600),
            details: Vec::new(),
        });
        let result = serialize_conversation([&assistant]);
        // A string argument is JSON text: the marker's newlines are escaped.
        for (letter, omitted) in [("w", 3000), ("e", 500)] {
            assert!(result.contains(&format!(
                "{}\\n\\n[... {omitted} more characters truncated]\"",
                letter.repeat(2000)
            )));
        }
        assert!(result.contains("limit=7"));
        assert!(result.contains("path=\"a.rs\""));
        assert!(result.contains(&format!(
            "[Assistant thinking]: {}\n\n[... 600 more characters truncated]\n\n",
            "t".repeat(2000)
        )));
        // The raw text of malformed arguments is bounded the same way.
        assert!(result.contains("more characters truncated]\")"));
        // The assistant's own text is kept whole.
        assert!(result.contains(&format!("[Assistant]: {file}\n\n")));
        assert!(!result.contains(&"m".repeat(2001)));
    }

    #[test]
    fn keeps_assistant_and_user_messages_whole() {
        let long = "y".repeat(5000);
        let messages = [
            ProviderMessage::user(long.clone()),
            ProviderMessage::assistant(long.clone(), Vec::new()),
        ];
        let result = serialize_conversation(&messages);

        assert!(!result.contains("truncated"));
        assert_eq!(result, format!("[User]: {long}\n\n[Assistant]: {long}"));
    }

    #[test]
    fn truncation_boundary_is_exactly_two_thousand_chars() {
        let exact = "z".repeat(2000);
        assert_eq!(truncate_for_summary(&exact, 2000), exact);
        let over = format!("{exact}z");
        assert_eq!(
            truncate_for_summary(&over, 2000),
            format!("{exact}\n\n[... 1 more characters truncated]")
        );
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        let text = "é".repeat(2500);
        let truncated = truncate_for_summary(&text, 2000);
        assert!(truncated.starts_with(&"é".repeat(2000)));
        assert!(truncated.ends_with("[... 500 more characters truncated]"));
    }

    #[test]
    fn renders_tool_calls_in_argument_order() {
        let call = ProviderToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: r#"{"path":"b.ts","limit":10,"flag":true,"note":"a\"b"}"#.into(),
        };
        let messages = [ProviderMessage::assistant("", vec![call])];
        assert_eq!(
            serialize_conversation(&messages),
            r#"[Assistant tool calls]: read(path="b.ts", limit=10, flag=true, note="a\"b")"#
        );
    }

    #[test]
    fn nested_objects_keep_their_source_order() {
        let call = ProviderToolCall {
            id: "c1".into(),
            name: "patch".into(),
            arguments: r#"{"path":"a.rs","edits":[{"replacement":"x","expected":"y"}],"opts":{"z":1,"a":{"m":null,"b":[true,false]}}}"#.into(),
        };
        assert_eq!(
            serialize_conversation(&[ProviderMessage::assistant("", vec![call])]),
            r#"[Assistant tool calls]: patch(path="a.rs", edits=[{"replacement":"x","expected":"y"}], opts={"z":1,"a":{"m":null,"b":[true,false]}})"#
        );
    }

    #[test]
    fn values_are_stringified_like_javascript() {
        let call = ProviderToolCall {
            id: "c1".into(),
            name: "t".into(),
            arguments: r#"{"a":1.0,"b":-0.0,"c":1e21,"d":1e-7,"e":2.5,"f":100000000000000000000,"g":"é\n\u0001","7":"seven","b2":{"2":1,"1":2,"x":3},"a":9}"#.into(),
        };
        // Integer-like keys come first; a repeated key keeps its first
        // position with its last value.
        assert_eq!(
            serialize_conversation(&[ProviderMessage::assistant("", vec![call])]),
            r#"[Assistant tool calls]: t(7="seven", a=9, b=0, c=1e+21, d=1e-7, e=2.5, f=100000000000000000000, g="é\n\u0001", b2={"1":2,"2":1,"x":3})"#
        );
    }

    #[test]
    fn joins_several_tool_calls_and_labels_every_part() {
        let calls = vec![
            ProviderToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: r#"{"path":"a.ts"}"#.into(),
            },
            ProviderToolCall {
                id: "c2".into(),
                name: "shell".into(),
                arguments: String::new(),
            },
        ];
        let mut assistant = ProviderMessage::assistant("Looking now.", calls);
        assistant.chat_reasoning = Some(crate::provider::ChatReasoning {
            scope_id: 1,
            model: "m".into(),
            content: "thinking hard".into(),
            details: Vec::new(),
        });
        let messages = [
            ProviderMessage::user("Please look"),
            assistant,
            ProviderMessage::tool("read", "c1", "file body"),
            ProviderMessage::tool("shell", "c2", ""),
        ];
        assert_eq!(
            serialize_conversation(&messages),
            "[User]: Please look\n\n[Assistant thinking]: thinking hard\n\n[Assistant]: Looking now.\n\n[Assistant tool calls]: read(path=\"a.ts\"); shell()\n\n[Tool result]: file body"
        );
    }

    #[test]
    fn malformed_arguments_are_kept_as_raw_text() {
        let call = ProviderToolCall {
            id: "c1".into(),
            name: "patch".into(),
            arguments: r#"{"path": "a.ts""#.into(),
        };
        let messages = [ProviderMessage::assistant("", vec![call])];
        assert_eq!(
            serialize_conversation(&messages),
            r#"[Assistant tool calls]: patch(arguments="{\"path\": \"a.ts\"")"#
        );
    }

    #[test]
    fn skips_system_messages_and_empty_content() {
        let mut system = ProviderMessage::user("rules");
        system.role = "system".into();
        let messages = [
            system,
            ProviderMessage::user(""),
            ProviderMessage::assistant("", Vec::new()),
            ProviderMessage::user("hi"),
        ];
        assert_eq!(serialize_conversation(&messages), "[User]: hi");
    }

    #[test]
    fn text_blocks_follow_the_content_and_attachments_add_nothing() {
        let message = ProviderMessage::user("first").with_content_blocks(vec![
            ProviderContentBlock::text("second"),
            ProviderContentBlock::image("image/png", "AAAA"),
        ]);
        assert_eq!(serialize_conversation([&message]), "[User]: firstsecond");
    }

    // Pi joins user and tool-result text blocks with "" and assistant text with a newline.
    #[test]
    fn text_blocks_are_joined_like_pis_content_text() {
        let tool = ProviderMessage::tool("read", "tc1", "one".to_owned())
            .with_content_blocks(vec![ProviderContentBlock::text("two")]);
        let assistant = ProviderMessage::assistant("a", Vec::new())
            .with_content_blocks(vec![ProviderContentBlock::text("b")]);
        assert_eq!(
            serialize_conversation([&tool, &assistant]),
            "[Tool result]: onetwo\n\n[Assistant]: a\nb"
        );
    }
}
