//! Token estimation and the compaction trigger.
//!
//! Port of Pi's `estimateTokens`, `estimateContextTokens`,
//! `calculateContextTokens` and `shouldCompact`
//! (`packages/coding-agent/src/core/compaction/compaction.ts`), Copyright (c)
//! 2025 Mario Zechner, MIT License (https://github.com/earendil-works/pi).
//!
//! Pi reads the usage of the last assistant message from its session entries.
//! Slim's provider messages carry no usage, so the caller hands the usage of
//! the last valid response in as a [`UsageAnchor`] holding the index of that
//! response in the messages.

use super::summary::last_compaction_summary_index;
use super::CompactionSettings;
use crate::provider::{ProviderContentBlock, ProviderMessage, UsageBreakdown};

/// Pi estimates every image as 4800 characters. Audio and file attachments are
/// opaque to the estimate in the same way, so they use the same figure.
pub(crate) const ESTIMATED_ATTACHMENT_CHARS: u64 = 4800;

/// Estimated tokens of one message: `ceil(chars / 4)` over its text, readable
/// reasoning text, tool-call names and arguments, plus 4800 characters per
/// attachment. There is no per-message overhead.
pub fn estimate_tokens(message: &ProviderMessage) -> u64 {
    let mut chars = message.content.chars().count() as u64;
    for block in &message.content_blocks {
        match block {
            ProviderContentBlock::Text(text) => chars += text.chars().count() as u64,
            ProviderContentBlock::Image { .. }
            | ProviderContentBlock::Audio { .. }
            | ProviderContentBlock::File { .. } => chars += ESTIMATED_ATTACHMENT_CHARS,
            ProviderContentBlock::Unsupported { .. } => {}
        }
    }
    for call in &message.tool_calls {
        chars += (call.name.chars().count() + call.arguments.chars().count()) as u64;
    }
    if let Some(reasoning) = &message.chat_reasoning {
        chars += reasoning.content.chars().count() as u64;
    }
    chars.div_ceil(4)
}

/// Estimated tokens of the system prompt and the tool schemas, which the
/// message estimate does not cover. Used only when no provider usage anchors
/// the estimate.
pub fn estimate_system_and_tools_tokens(system_prompt: &str, tools: &[serde_json::Value]) -> u64 {
    let tool_chars: u64 = tools.iter().map(json_chars).sum();
    (system_prompt.chars().count() as u64)
        .saturating_add(tool_chars)
        .div_ceil(4)
}

/// Characters of the compact JSON text of `value` (what `to_string()` would
/// produce), counted without building it: a tool set is large and fixed for a
/// run.
fn json_chars(value: &serde_json::Value) -> u64 {
    use serde_json::Value;
    match value {
        Value::Null | Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => {
            use std::fmt::Write as _;
            struct Count(u64);
            impl std::fmt::Write for Count {
                fn write_str(&mut self, text: &str) -> std::fmt::Result {
                    self.0 += text.chars().count() as u64;
                    Ok(())
                }
            }
            let mut count = Count(0);
            let _ = write!(count, "{number}");
            count.0
        }
        Value::String(text) => json_string_chars(text),
        Value::Array(items) => items.iter().map(json_chars).fold(
            2 + items.len().saturating_sub(1) as u64,
            u64::saturating_add,
        ),
        Value::Object(entries) => entries.iter().fold(
            2 + entries.len().saturating_sub(1) as u64,
            |total, (key, value)| {
                total
                    .saturating_add(json_string_chars(key))
                    .saturating_add(1)
                    .saturating_add(json_chars(value))
            },
        ),
    }
}

/// Characters of a JSON string literal, quotes and escapes included.
fn json_string_chars(text: &str) -> u64 {
    text.chars().fold(2_u64, |total, character| {
        total
            + match character {
                '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
                control if control < ' ' => 6,
                _ => 1,
            }
    })
}

/// Context size reported by a provider for a response: its input (fresh, cache
/// reads and cache writes) plus its output.
pub fn calculate_context_tokens(usage: &UsageBreakdown) -> u64 {
    usage
        .total_input_tokens()
        .saturating_add(usage.output_tokens)
}

/// The usage of the last valid response and where that response sits in the
/// messages being estimated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageAnchor {
    /// Index of the response's assistant message.
    pub message_index: usize,
    /// Context tokens the provider reported for that response.
    pub context_tokens: u64,
}

impl UsageAnchor {
    /// An anchor from a response's usage. Aborted or failed responses and
    /// responses without usable usage (unknown or all zero) give `None`, as in
    /// Pi.
    pub fn from_breakdown(message_index: usize, usage: &UsageBreakdown) -> Option<Self> {
        let context_tokens = calculate_context_tokens(usage);
        (!usage.usage_unknown && context_tokens > 0).then_some(Self {
            message_index,
            context_tokens,
        })
    }
}

/// What the context estimate may rely on besides the messages themselves.
///
/// The caller must not pass an anchor taken before the latest compaction or
/// any other edit of the messages that changed the context: the numbers no
/// longer describe it. [`estimate_context_tokens`] additionally ignores an
/// anchor that does not lie after the latest compaction summary message.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextUsage {
    pub anchor: Option<UsageAnchor>,
    /// Estimate of the system prompt and tool schemas (see
    /// [`estimate_system_and_tools_tokens`]), added when there is no anchor.
    /// An anchored usage already includes them.
    pub fixed_tokens: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    /// Provider-reported part of the estimate (0 without an anchor).
    pub usage_tokens: u64,
    /// Estimated part: the messages after the anchor, or all of them plus the
    /// fixed tokens without an anchor.
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}

/// The anchor when it can still be trusted over `messages`: it points at a
/// message, and that message comes after the latest compaction summary.
pub fn usable_anchor(
    messages: &[ProviderMessage],
    anchor: Option<UsageAnchor>,
) -> Option<UsageAnchor> {
    let after_compaction = last_compaction_summary_index(messages).map_or(0, |index| index + 1);
    anchor.filter(|anchor| {
        anchor.context_tokens > 0
            && anchor.message_index < messages.len()
            && anchor.message_index >= after_compaction
    })
}

/// Context tokens from the last response's usage plus an estimate of the
/// messages after it. Without a trusted anchor, everything is estimated: the
/// messages and the fixed system/tool tokens.
pub fn estimate_context_tokens(
    messages: &[ProviderMessage],
    usage: ContextUsage,
) -> ContextUsageEstimate {
    match usable_anchor(messages, usage.anchor) {
        Some(anchor) => {
            let trailing_tokens = messages[anchor.message_index + 1..]
                .iter()
                .map(estimate_tokens)
                .sum::<u64>();
            ContextUsageEstimate {
                tokens: anchor.context_tokens.saturating_add(trailing_tokens),
                usage_tokens: anchor.context_tokens,
                trailing_tokens,
                last_usage_index: Some(anchor.message_index),
            }
        }
        None => {
            let estimated = messages
                .iter()
                .map(estimate_tokens)
                .sum::<u64>()
                .saturating_add(usage.fixed_tokens);
            ContextUsageEstimate {
                tokens: estimated,
                usage_tokens: 0,
                trailing_tokens: estimated,
                last_usage_index: None,
            }
        }
    }
}

/// Whether compaction should run: enabled and the context above
/// `context_window - reserve_tokens` (strictly, as in Pi).
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    i128::from(context_tokens) > i128::from(context_window) - i128::from(settings.reserve_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderToolCall;

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> UsageBreakdown {
        UsageBreakdown {
            uncached_input_tokens: input,
            cache_write_tokens: cache_write,
            cache_read_tokens: cache_read,
            output_tokens: output,
            reasoning_tokens: 0,
            usage_unknown: false,
        }
    }

    fn settings(enabled: bool, reserve_tokens: u64) -> CompactionSettings {
        CompactionSettings {
            enabled,
            reserve_tokens,
            keep_recent_tokens: 20_000,
        }
    }

    // Pi: compaction.test.ts "Token calculation"
    #[test]
    fn calculates_total_context_tokens_from_usage() {
        assert_eq!(calculate_context_tokens(&usage(1000, 500, 200, 100)), 1800);
        assert_eq!(calculate_context_tokens(&usage(0, 0, 0, 0)), 0);
    }

    #[test]
    fn anchors_skip_unknown_and_all_zero_usage() {
        assert_eq!(
            UsageAnchor::from_breakdown(3, &usage(100, 50, 0, 0)),
            Some(UsageAnchor {
                message_index: 3,
                context_tokens: 150
            })
        );
        assert_eq!(UsageAnchor::from_breakdown(3, &usage(0, 0, 0, 0)), None);
        let mut unknown = usage(100, 50, 0, 0);
        unknown.usage_unknown = true;
        assert_eq!(UsageAnchor::from_breakdown(3, &unknown), None);
    }

    // Pi: estimateTokens
    #[test]
    fn estimates_chars_over_four_rounded_up() {
        assert_eq!(estimate_tokens(&ProviderMessage::user("")), 0);
        assert_eq!(estimate_tokens(&ProviderMessage::user("a")), 1);
        assert_eq!(estimate_tokens(&ProviderMessage::user("abcd")), 1);
        assert_eq!(estimate_tokens(&ProviderMessage::user("abcde")), 2);
        assert_eq!(
            estimate_tokens(&ProviderMessage::tool("read", "c1", "x".repeat(8000))),
            2000
        );
    }

    #[test]
    fn estimates_tool_calls_by_name_and_arguments() {
        let call = ProviderToolCall {
            id: "a-long-call-identifier-that-is-not-counted".into(),
            name: "read".into(),
            arguments: r#"{"path":"a.rs"}"#.into(),
        };
        let message = ProviderMessage::assistant("", vec![call]);
        // 4 + 15 characters
        assert_eq!(estimate_tokens(&message), 5);
    }

    #[test]
    fn estimates_text_blocks_and_attachments() {
        let message = ProviderMessage::user("abcd").with_content_blocks(vec![
            ProviderContentBlock::text("efgh"),
            ProviderContentBlock::image("image/png", "A".repeat(1_000_000)),
            ProviderContentBlock::Unsupported { kind: "x".into() },
        ]);
        assert_eq!(estimate_tokens(&message), (4 + 4 + 4800) / 4);
    }

    #[test]
    fn estimates_readable_reasoning_text() {
        let mut message = ProviderMessage::assistant("abcd", Vec::new());
        message.chat_reasoning = Some(crate::provider::ChatReasoning {
            scope_id: 1,
            model: "m".into(),
            content: "efghijkl".into(),
            details: Vec::new(),
        });
        assert_eq!(estimate_tokens(&message), 3);
    }

    // Pi: compaction.test.ts "estimateContextTokens"
    #[test]
    fn uses_the_anchor_as_the_context_base() {
        let messages = [
            ProviderMessage::user("Hello"),
            ProviderMessage::assistant("Hi", Vec::new()),
            ProviderMessage::user("continue"),
            ProviderMessage::assistant("Partial thinking", Vec::new()),
        ];
        let estimate = estimate_context_tokens(
            &messages,
            ContextUsage {
                anchor: UsageAnchor::from_breakdown(1, &usage(100, 50, 0, 0)),
                fixed_tokens: 9_999,
            },
        );

        assert_eq!(estimate.usage_tokens, 150);
        assert_eq!(estimate.last_usage_index, Some(1));
        assert_eq!(
            estimate.trailing_tokens,
            estimate_tokens(&messages[2]) + estimate_tokens(&messages[3])
        );
        assert!(estimate.trailing_tokens > 0);
        assert_eq!(estimate.tokens, 150 + estimate.trailing_tokens);
    }

    #[test]
    fn estimates_everything_and_adds_fixed_tokens_without_an_anchor() {
        let messages = [
            ProviderMessage::user("abcd"),
            ProviderMessage::assistant("abcdefgh", Vec::new()),
        ];
        let estimate = estimate_context_tokens(
            &messages,
            ContextUsage {
                anchor: None,
                fixed_tokens: 100,
            },
        );
        assert_eq!(estimate.tokens, 103);
        assert_eq!(estimate.usage_tokens, 0);
        assert_eq!(estimate.trailing_tokens, 103);
        assert_eq!(estimate.last_usage_index, None);
    }

    #[test]
    fn ignores_an_unusable_anchor() {
        let messages = [
            ProviderMessage::user("abcd"),
            ProviderMessage::assistant("abcd", Vec::new()),
        ];
        let unanchored = estimate_context_tokens(&messages, ContextUsage::default());
        for anchor in [
            UsageAnchor {
                message_index: 1,
                context_tokens: 0,
            },
            UsageAnchor {
                message_index: 2,
                context_tokens: 500,
            },
        ] {
            assert_eq!(
                estimate_context_tokens(
                    &messages,
                    ContextUsage {
                        anchor: Some(anchor),
                        fixed_tokens: 0
                    }
                ),
                unanchored
            );
        }
    }

    #[test]
    fn does_not_trust_usage_from_before_the_latest_compaction() {
        let messages = [
            super::super::compaction_summary_message("summary"),
            ProviderMessage::user("abcd"),
            ProviderMessage::assistant("abcd", Vec::new()),
            ProviderMessage::user("abcd"),
        ];
        let stale = UsageAnchor {
            message_index: 0,
            context_tokens: 90_000,
        };
        let at_summary = estimate_context_tokens(
            &messages,
            ContextUsage {
                anchor: Some(stale),
                fixed_tokens: 7,
            },
        );
        assert_eq!(at_summary.usage_tokens, 0);
        assert_eq!(
            at_summary.tokens,
            messages.iter().map(estimate_tokens).sum::<u64>() + 7
        );

        let fresh = estimate_context_tokens(
            &messages,
            ContextUsage {
                anchor: Some(UsageAnchor {
                    message_index: 2,
                    context_tokens: 1_000,
                }),
                fixed_tokens: 7,
            },
        );
        assert_eq!(fresh.tokens, 1_001);
    }

    #[test]
    fn system_and_tools_estimate_covers_prompt_and_schemas() {
        let tools = [serde_json::json!({"name": "read"})];
        let tool_chars = tools[0].to_string().chars().count();
        assert_eq!(
            estimate_system_and_tools_tokens("abcd", &tools),
            ((4 + tool_chars) as u64).div_ceil(4)
        );
        assert_eq!(estimate_system_and_tools_tokens("", &[]), 0);
    }

    #[test]
    fn counting_json_chars_matches_the_serialized_text() {
        for value in [
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(false),
            serde_json::json!(-12.5),
            serde_json::json!(1_000_000_u64),
            serde_json::json!("quote \" slash \\ tab \t nl \n bell \u{7} \u{1}- ação \u{1f600}"),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!([1, [2, {"a": null}], "x"]),
            serde_json::json!({
                "name": "read",
                "description": "Reads \"a\" file",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "n": {"type": "integer", "minimum": 0}}, "required": ["path"]}
            }),
        ] {
            assert_eq!(
                json_chars(&value),
                value.to_string().chars().count() as u64,
                "{value}"
            );
        }
    }

    #[test]
    fn an_anchor_before_the_latest_summary_is_not_usable() {
        let messages = [
            ProviderMessage::user("a"),
            ProviderMessage::assistant("b", Vec::new()),
            crate::context::compaction_summary_message("s"),
            ProviderMessage::assistant("c", Vec::new()),
        ];
        let anchor = |message_index| {
            Some(UsageAnchor {
                message_index,
                context_tokens: 10,
            })
        };
        assert_eq!(usable_anchor(&messages, anchor(1)), None);
        assert_eq!(usable_anchor(&messages, anchor(2)), None);
        assert_eq!(usable_anchor(&messages, anchor(3)), anchor(3));
        assert_eq!(usable_anchor(&messages, anchor(9)), None);
        assert_eq!(usable_anchor(&messages, None), None);
    }

    // Pi: compaction.test.ts "shouldCompact"
    #[test]
    fn compacts_only_above_the_threshold() {
        let settings = settings(true, 10_000);
        assert!(should_compact(95_000, 100_000, &settings));
        assert!(!should_compact(89_000, 100_000, &settings));
        assert!(!should_compact(90_000, 100_000, &settings));
        assert!(should_compact(90_001, 100_000, &settings));
    }

    #[test]
    fn does_not_compact_when_disabled() {
        assert!(!should_compact(95_000, 100_000, &settings(false, 10_000)));
    }

    #[test]
    fn a_reserve_beyond_the_window_always_compacts() {
        assert!(should_compact(0, 8_000, &settings(true, 16_384)));
    }
}
