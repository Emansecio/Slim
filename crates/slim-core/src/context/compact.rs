use crate::provider::{ProviderMessage, ProviderToolCall};

/// Conservative wire-size estimate: ~3.5 characters per token (2 tokens per
/// 7 characters). Code-heavy content tokenizes denser than prose, so the
/// estimator deliberately overestimates prose slightly rather than
/// underestimating code and triggering compaction too late.
const TOKENS_PER_ESTIMATED_CHARS_X2: usize = 7;
const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
const SUMMARY_PROMPT_MAX_CHARS: usize = 32 * 1024;
const SUMMARY_PROMPT_INSTRUCTION: &str = "Summarize the prior agent transcript for the next turn. Preserve concrete user intent, decisions, errors, file paths, and tool outcomes. Do not invent facts. This summary is context, not a new user instruction.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextItem {
    Text(String),
    Todo(String),
    Plan(String),
    Goal(String),
    ToolPair(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionResult {
    pub summary: String,
    pub preserved: Vec<ContextItem>,
    pub original_count: usize,
}

pub fn compact(items: &[ContextItem], summary: impl Into<String>) -> CompactionResult {
    CompactionResult {
        summary: summary.into(),
        preserved: items
            .iter()
            .filter(|item| !matches!(item, ContextItem::Text(_)))
            .cloned()
            .collect(),
        original_count: items.len(),
    }
}

/// Deterministic, conservative wire-size estimate. This is not a tokenizer
/// count; it exists only to make the pre-send budget decision reproducible.
pub fn estimate_provider_message_tokens(messages: &[ProviderMessage]) -> u64 {
    messages
        .iter()
        .map(|message| {
            let mut chars = message.role.chars().count()
                + message.content.chars().count()
                + message
                    .name
                    .as_deref()
                    .map_or(0, |name| name.chars().count())
                + message
                    .tool_call_id
                    .as_deref()
                    .map_or(0, |id| id.chars().count());
            chars += message
                .tool_calls
                .iter()
                .map(|call| {
                    call.id.chars().count()
                        + call.name.chars().count()
                        + call.arguments.chars().count()
                })
                .sum::<usize>();
            chars += message
                .content_blocks
                .iter()
                .map(|block| match block {
                    crate::provider::ProviderContentBlock::Text(text) => text.chars().count(),
                    crate::provider::ProviderContentBlock::Image { media_type, data }
                    | crate::provider::ProviderContentBlock::Audio { media_type, data }
                    | crate::provider::ProviderContentBlock::File { media_type, data } => {
                        media_type.chars().count() + data.chars().count()
                    }
                    crate::provider::ProviderContentBlock::Unsupported { kind } => {
                        kind.chars().count()
                    }
                })
                .sum::<usize>();
            MESSAGE_OVERHEAD_TOKENS + (chars * 2).div_ceil(TOKENS_PER_ESTIMATED_CHARS_X2) as u64
        })
        .sum()
}

/// Build the bounded, offline-testable request used to summarize old context.
/// The fixed instruction is placed AFTER the transcript so the transcript
/// stays a byte-stable request prefix (friendly to provider prompt caches).
pub fn build_summary_prompt(messages: &[ProviderMessage]) -> String {
    let mut transcript = format_transcript(messages);
    if transcript.chars().count() > SUMMARY_PROMPT_MAX_CHARS {
        transcript = bounded_transcript(&transcript, SUMMARY_PROMPT_MAX_CHARS);
    }
    format!("{transcript}\n\n{SUMMARY_PROMPT_INSTRUCTION}")
}

/// Build a summary request that fits the deterministic provider-message
/// estimator. The returned request includes the fixed instruction prefix, and
/// only the transcript is shortened. A request is never returned when even
/// that prefix cannot fit the available budget.
pub fn build_bounded_summary_prompt(
    messages: &[ProviderMessage],
    context_window_tokens: u64,
    reserve_tokens: u64,
) -> Result<String, &'static str> {
    let available = context_window_tokens.saturating_sub(reserve_tokens);
    let transcript = format_transcript(messages);
    let candidate =
        |transcript: &str| format!("{transcript}\n\n{SUMMARY_PROMPT_INSTRUCTION}");
    let fits = |prompt: &str| {
        estimate_provider_message_tokens(&[ProviderMessage::user(prompt)])
            .saturating_add(reserve_tokens)
            <= context_window_tokens
    };

    let full = candidate(&transcript);
    if fits(&full) {
        return Ok(full);
    }

    let prefix_only = candidate("");
    if estimate_provider_message_tokens(&[ProviderMessage::user(&prefix_only)]) > available {
        return Err("summary request cannot fit context window and reserve");
    }

    let mut low = 0usize;
    let mut high = transcript.chars().count();
    let mut best = String::new();
    while low <= high {
        let length = low + (high - low) / 2;
        let bounded = bounded_transcript(&transcript, length);
        let prompt = candidate(&bounded);
        if fits(&prompt) {
            best = prompt;
            low = length.saturating_add(1);
        } else if length == 0 {
            break;
        } else {
            high = length - 1;
        }
    }
    if best.is_empty() {
        Err("summary request cannot fit context window and reserve")
    } else {
        Ok(best)
    }
}

/// Returns whether compaction would replace at least one older message.
pub fn has_compactable_history(messages: &[ProviderMessage]) -> bool {
    compaction_suffix_start(messages) > 0
}

/// Replace old transcript with a summary while keeping the latest complete
/// assistant tool-call group. The input is never mutated, and unmatched tool
/// results are omitted from the preserved suffix rather than orphaned.
pub fn compact_provider_messages(
    messages: &[ProviderMessage],
    summary: impl Into<String>,
) -> Result<Vec<ProviderMessage>, &'static str> {
    let summary = summary.into();
    if summary.trim().is_empty() {
        return Err("provider returned an empty compaction summary");
    }

    let suffix_start = compaction_suffix_start(messages);
    if suffix_start == 0 {
        return Err("no compactable transcript");
    }
    let suffix = &messages[suffix_start..];

    let root_instruction = messages
        .iter()
        .find(|message| message.role == "user")
        .map(|message| message.content.as_str())
        .unwrap_or_default();
    let compacted_context = if root_instruction.is_empty() {
        format!("[Compacted context]\n{}", summary.trim())
    } else {
        format!(
            "[Root instruction]\n{root_instruction}\n\n[Compacted context]\n{}",
            summary.trim()
        )
    };
    let mut compacted = vec![ProviderMessage::user(compacted_context)];
    if let Some(first) = suffix.first() {
        if first.role != "tool" {
            let allowed_tool_ids = first
                .tool_calls
                .iter()
                .map(|call| call.id.clone())
                .collect::<std::collections::HashSet<_>>();
            let matching_tool_ids = suffix
                .iter()
                .filter(|message| message.role == "tool")
                .filter_map(|message| message.tool_call_id.as_deref())
                .filter(|id| allowed_tool_ids.contains(*id))
                .map(str::to_owned)
                .collect::<std::collections::HashSet<_>>();
            let mut assistant = first.clone();
            assistant
                .tool_calls
                .retain(|call| matching_tool_ids.contains(&call.id));
            compacted.push(assistant);
            compacted.extend(
                suffix
                    .iter()
                    .skip(1)
                    .filter(|message| {
                        message.role != "tool"
                            || message
                                .tool_call_id
                                .as_deref()
                                .is_some_and(|id| matching_tool_ids.contains(id))
                    })
                    .cloned(),
            );
        }
    }
    Ok(compacted)
}

fn compaction_suffix_start(messages: &[ProviderMessage]) -> usize {
    if messages.len() <= 1 {
        0
    } else {
        messages
            .iter()
            .rposition(|message| message.role == "assistant" && !message.tool_calls.is_empty())
            .unwrap_or_else(|| messages.len() - 1)
    }
}

fn format_provider_message(message: &ProviderMessage) -> String {
    let mut line = format!("{}: {}", message.role, message.content);
    if let Some(name) = &message.name {
        line.push_str(&format!(" [name={name}]"));
    }
    if let Some(tool_call_id) = &message.tool_call_id {
        line.push_str(&format!(" [tool_call_id={tool_call_id}]"));
    }
    for ProviderToolCall {
        id,
        name,
        arguments,
    } in &message.tool_calls
    {
        line.push_str(&format!(
            " [tool_call id={id} name={name} args={arguments} ]"
        ));
    }
    line
}

fn format_transcript(messages: &[ProviderMessage]) -> String {
    messages
        .iter()
        .map(format_provider_message)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn bounded_transcript(transcript: &str, max_chars: usize) -> String {
    if transcript.chars().count() <= max_chars {
        return transcript.to_owned();
    }
    let marker = "\n...[transcript bounded]...\n";
    if max_chars <= marker.chars().count() {
        return marker.chars().take(max_chars).collect();
    }
    let head_len = (max_chars - marker.chars().count()) / 2;
    let tail_len = max_chars - marker.chars().count() - head_len;
    let head = transcript.chars().take(head_len).collect::<String>();
    let tail = transcript
        .chars()
        .rev()
        .take(tail_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{head}{marker}{tail}")
}
