//! Summary requests, the summary message and its assembly.
//!
//! Port of Pi's `generateSummaryWithUsage`, `generateTurnPrefixSummary`, the
//! split-turn merge in `compact`, and the `compactionSummary` conversion in
//! `messages.ts` (`packages/coding-agent/src/core/`), Copyright (c) 2025 Mario
//! Zechner, MIT License (https://github.com/earendil-works/pi). Everything here
//! is pure: the runtime sends the requests and feeds the answers back.

use super::cut::CompactionPreparation;
use super::prompts::{
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, SUMMARIZATION_PROMPT,
    SUMMARIZATION_SYSTEM_PROMPT, TURN_PREFIX_SUMMARIZATION_PROMPT, UPDATE_SUMMARIZATION_PROMPT,
};
use super::serialize::serialize_conversation;
use crate::provider::ProviderMessage;

/// Stands in for the history summary of a split turn that has neither history
/// nor a previous summary.
pub const NO_PRIOR_HISTORY: &str = "No prior history.";

/// One summarization call: a single user message under the summarization
/// system prompt. Tools, prompt-cache writes and sampling options are the
/// caller's to switch off, as in Pi.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SummaryRequest {
    pub system_prompt: &'static str,
    /// Text of the only user message.
    pub prompt: String,
    /// Output cap derived from the reserve. The model's own limit lowers it
    /// when the request is prepared for the wire
    /// (`HttpProviderClient::prepare_compaction_messages`).
    pub max_output_tokens: u64,
    /// Byte range of the serialized conversation inside `prompt`.
    conversation: std::ops::Range<usize>,
}

/// Replaces the middle of an oversized conversation.
const CONVERSATION_BOUNDED_MARKER: &str = "\n...[conversation bounded]...\n";

impl SummaryRequest {
    /// The request's messages (the system prompt travels separately).
    pub fn messages(&self) -> Vec<ProviderMessage> {
        vec![ProviderMessage::user(self.prompt.clone())]
    }

    /// Bytes the prompt can lose to [`Self::bound_conversation`].
    pub fn conversation_bytes(&self) -> usize {
        self.conversation.len()
    }

    /// Shrinks the prompt to at most `max_prompt_bytes` by keeping the head
    /// and the tail of the serialized conversation around a marker. A prompt
    /// that already fits is untouched. The fixed parts of the prompt (the
    /// instructions and the previous summary) are never cut; when they alone
    /// exceed the limit the conversation is reduced to the marker.
    pub fn bound_conversation(&mut self, max_prompt_bytes: usize) {
        if self.prompt.len() <= max_prompt_bytes {
            return;
        }
        let conversation = &self.prompt[self.conversation.clone()];
        let fixed = self.prompt.len() - conversation.len();
        let keep = max_prompt_bytes
            .saturating_sub(fixed)
            .saturating_sub(CONVERSATION_BOUNDED_MARKER.len());
        let head_end = floor_char_boundary(conversation, keep / 2);
        let tail_start = ceil_char_boundary(
            conversation,
            conversation.len().saturating_sub(keep - keep / 2),
        );
        let bounded = if head_end >= tail_start {
            conversation.to_owned()
        } else {
            format!(
                "{}{CONVERSATION_BOUNDED_MARKER}{}",
                &conversation[..head_end],
                &conversation[tail_start..]
            )
        };
        let end = self.conversation.end;
        self.conversation = self.conversation.start..self.conversation.start + bounded.len();
        self.prompt
            .replace_range(self.conversation.start..end, &bounded);
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// The calls a compaction needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SummaryRequests {
    /// The history summary. `None` when a split turn has no history of its own
    /// (the previous summary is then carried over unchanged).
    pub history: Option<SummaryRequest>,
    /// The summary of a split turn's prefix.
    pub turn_prefix: Option<SummaryRequest>,
}

/// Output cap of a summary call: `percent` of the reserve.
fn max_output_tokens(reserve_tokens: u64, percent: u64) -> u64 {
    reserve_tokens.saturating_mul(percent) / 100
}

/// Request for the history summary: the first summary, or an update of
/// `previous_summary` when there is one. Empty instructions or an empty
/// previous summary count as absent, as in Pi.
pub fn build_history_summary_request<'a>(
    messages: impl IntoIterator<Item = &'a ProviderMessage>,
    reserve_tokens: u64,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
) -> SummaryRequest {
    let previous_summary = previous_summary.filter(|summary| !summary.is_empty());
    let mut base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT.to_owned()
    } else {
        SUMMARIZATION_PROMPT.to_owned()
    };
    if let Some(instructions) = custom_instructions.filter(|text| !text.is_empty()) {
        base_prompt = format!("{base_prompt}\n\nAdditional focus: {instructions}");
    }

    let conversation = serialize_conversation(messages);
    let conversation_start = "<conversation>\n".len();
    let conversation_range = conversation_start..conversation_start + conversation.len();
    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(summary) = previous_summary {
        prompt.push_str(&format!(
            "<previous-summary>\n{summary}\n</previous-summary>\n\n"
        ));
    }
    prompt.push_str(&base_prompt);

    SummaryRequest {
        system_prompt: SUMMARIZATION_SYSTEM_PROMPT,
        prompt,
        max_output_tokens: max_output_tokens(reserve_tokens, 80),
        conversation: conversation_range,
    }
}

/// Request for the summary of a split turn's prefix. It gets half of the
/// history summary's budget.
pub fn build_turn_prefix_summary_request<'a>(
    messages: impl IntoIterator<Item = &'a ProviderMessage>,
    reserve_tokens: u64,
) -> SummaryRequest {
    let conversation = serialize_conversation(messages);
    let conversation_start = "# Conversation\n".len();
    SummaryRequest {
        system_prompt: SUMMARIZATION_SYSTEM_PROMPT,
        conversation: conversation_start..conversation_start + conversation.len(),
        prompt: format!(
            "# Conversation\n{conversation}\n\n# Instructions\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
        ),
        max_output_tokens: max_output_tokens(reserve_tokens, 50),
    }
}

/// Joins the history summary and the turn-prefix summary of a split turn.
pub fn merge_split_turn_summary(history_summary: &str, turn_prefix_summary: &str) -> String {
    format!("{history_summary}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_summary}")
}

impl CompactionPreparation {
    /// Whether the compaction summarizes a split turn's prefix separately.
    fn summarizes_turn_prefix(&self) -> bool {
        self.is_split_turn && !self.turn_prefix_messages.is_empty()
    }

    /// The calls this compaction needs. A split turn also gets the turn-prefix
    /// call, and skips the history call when it has no history of its own.
    pub fn summary_requests(&self, custom_instructions: Option<&str>) -> SummaryRequests {
        let reserve_tokens = self.settings.reserve_tokens;
        let history = |this: &Self| {
            build_history_summary_request(
                &this.messages_to_summarize,
                reserve_tokens,
                custom_instructions,
                this.previous_summary.as_deref(),
            )
        };
        if self.summarizes_turn_prefix() {
            SummaryRequests {
                history: (!self.messages_to_summarize.is_empty()).then(|| history(self)),
                turn_prefix: Some(build_turn_prefix_summary_request(
                    &self.turn_prefix_messages,
                    reserve_tokens,
                )),
            }
        } else {
            SummaryRequests {
                history: Some(history(self)),
                turn_prefix: None,
            }
        }
    }

    /// The summary text from the answers to [`Self::summary_requests`], before
    /// the file lists are appended. A split turn merges both answers and, when
    /// it had no history call, carries the previous summary over (or
    /// [`NO_PRIOR_HISTORY`]).
    pub fn assemble_summary(
        &self,
        history_text: Option<&str>,
        turn_prefix_text: Option<&str>,
    ) -> Result<String, &'static str> {
        if self.summarizes_turn_prefix() {
            let history = if self.messages_to_summarize.is_empty() {
                self.previous_summary.as_deref().unwrap_or(NO_PRIOR_HISTORY)
            } else {
                history_text.ok_or("missing history summary")?
            };
            let turn_prefix = turn_prefix_text.ok_or("missing turn prefix summary")?;
            Ok(merge_split_turn_summary(history, turn_prefix))
        } else {
            history_text
                .map(str::to_owned)
                .ok_or("missing history summary")
        }
    }
}

/// Content of the user message that stands in for compacted history.
pub fn compaction_summary_content(summary: &str) -> String {
    format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}")
}

/// Prefix of the pointer that replaces a tool result identical to one already
/// in context (`runtime::history_elision`).
pub const DUPLICATE_POINTER_PREFIX: &str = "[duplicate ";

/// Prefix of the pointer that replaces a tool output a later mutation made
/// obsolete.
const SUPERSEDED_POINTER_PREFIX: &str = "[superseded ";

/// Rewrites the duplicate-result pointers in `kept` that no longer point at
/// anything.
///
/// A duplicate pointer names only its tool, and says the identical output is
/// already in context. When a compaction summarizes the original output away
/// and keeps the pointer, that claim is false and the model would trust
/// content it does not have. A pointer stays only while an earlier kept result
/// of the same tool still holds a full output; every other one says the output
/// was compacted away. The match is by tool name, so an earlier result of the
/// same tool that is a different output keeps a pointer that should have been
/// rewritten; the rewrite errs the other way only. The journal keeps the
/// original text (`recorded_content`), so its fingerprint is unchanged.
pub fn rewrite_stale_duplicate_pointers(kept: &mut [ProviderMessage]) {
    let mut with_full_output = std::collections::HashSet::<String>::new();
    for message in kept.iter_mut() {
        if message.role != "tool" {
            continue;
        }
        let Some(name) = message.name.clone() else {
            continue;
        };
        if message.content.starts_with(DUPLICATE_POINTER_PREFIX) {
            if with_full_output.contains(&name) {
                continue;
            }
            let note = compacted_duplicate_note(&name);
            if message.content != note {
                message
                    .recorded_content
                    .get_or_insert_with(|| std::sync::Arc::from(message.content.as_str()));
                message.content = note;
            }
        } else if !message.content.starts_with(SUPERSEDED_POINTER_PREFIX) {
            with_full_output.insert(name);
        }
    }
}

fn compacted_duplicate_note(tool_name: &str) -> String {
    format!(
        "{DUPLICATE_POINTER_PREFIX}{tool_name} result omitted; the identical output was compacted away; call the tool again if you need it]"
    )
}

/// The user message the model sees in place of the compacted history.
pub fn compaction_summary_message(summary: &str) -> ProviderMessage {
    ProviderMessage::user(compaction_summary_content(summary))
}

/// The summary carried by a compaction summary message, `None` for any other
/// message.
pub fn compaction_summary_text(message: &ProviderMessage) -> Option<&str> {
    if message.role != "user" || !message.content_blocks.is_empty() {
        return None;
    }
    message
        .content
        .strip_prefix(COMPACTION_SUMMARY_PREFIX)?
        .strip_suffix(COMPACTION_SUMMARY_SUFFIX)
}

/// Index of the latest compaction summary message.
pub(super) fn last_compaction_summary_index(messages: &[ProviderMessage]) -> Option<usize> {
    messages
        .iter()
        .rposition(|message| compaction_summary_text(message).is_some())
}

/// The history after compacting: the system and developer messages found
/// before the cut (they are not conversation and never summarized), the
/// summary message, and the messages from `first_kept_index` on.
pub fn apply_compaction(
    messages: &[ProviderMessage],
    first_kept_index: usize,
    summary: &str,
) -> Vec<ProviderMessage> {
    let first_kept_index = first_kept_index.min(messages.len());
    let mut compacted: Vec<ProviderMessage> = messages[..first_kept_index]
        .iter()
        .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
        .cloned()
        .collect();
    compacted.push(compaction_summary_message(summary));
    let kept_from = compacted.len();
    compacted.extend(messages[first_kept_index..].iter().cloned());
    rewrite_stale_duplicate_pointers(&mut compacted[kept_from..]);
    compacted
}

#[cfg(test)]
mod tests {
    use super::super::estimate::ContextUsage;
    use super::super::file_ops::FileOperations;
    use super::super::{prepare_compaction, CompactionSettings, DEFAULT_COMPACTION_SETTINGS};
    use super::*;

    fn preparation(
        messages_to_summarize: Vec<ProviderMessage>,
        turn_prefix_messages: Vec<ProviderMessage>,
        is_split_turn: bool,
        previous_summary: Option<&str>,
        reserve_tokens: u64,
    ) -> CompactionPreparation {
        CompactionPreparation {
            first_kept_index: 0,
            messages_to_summarize,
            turn_prefix_messages,
            is_split_turn,
            tokens_before: 100,
            previous_summary: previous_summary.map(str::to_owned),
            file_ops: FileOperations::new(),
            settings: CompactionSettings {
                enabled: true,
                reserve_tokens,
                keep_recent_tokens: 20,
            },
        }
    }

    #[test]
    fn the_first_summary_prompt_wraps_the_conversation() {
        let messages = [ProviderMessage::user("Summarize this.")];
        let request = build_history_summary_request(&messages, 16_384, None, None);
        assert_eq!(request.system_prompt, SUMMARIZATION_SYSTEM_PROMPT);
        assert_eq!(
            request.prompt,
            format!("<conversation>\n[User]: Summarize this.\n</conversation>\n\n{SUMMARIZATION_PROMPT}")
        );
        assert_eq!(
            request.messages(),
            vec![ProviderMessage::user(request.prompt.as_str())]
        );
    }

    #[test]
    fn an_update_prompt_carries_the_previous_summary() {
        let messages = [ProviderMessage::user("more work")];
        let request = build_history_summary_request(&messages, 16_384, None, Some("## Goal\nold"));
        assert_eq!(
            request.prompt,
            format!(
                "<conversation>\n[User]: more work\n</conversation>\n\n<previous-summary>\n## Goal\nold\n</previous-summary>\n\n{UPDATE_SUMMARIZATION_PROMPT}"
            )
        );
    }

    #[test]
    fn an_oversized_conversation_is_bounded_head_and_tail_only_when_needed() {
        let text = format!("{}{}", "α".repeat(400), "ω".repeat(400));
        let messages = [ProviderMessage::user(text)];
        let request = build_history_summary_request(&messages, 100, None, Some("old"));

        let mut fits = request.clone();
        fits.bound_conversation(request.prompt.len());
        assert_eq!(fits, request);

        let limit = request.prompt.len() - 300;
        let mut bounded = request.clone();
        bounded.bound_conversation(limit);
        assert!(bounded.prompt.len() <= limit);
        assert!(bounded.prompt.contains("...[conversation bounded]..."));
        assert!(bounded.prompt.starts_with("<conversation>\n[User]: α"));
        assert!(bounded
            .prompt
            .contains("ω\n</conversation>\n\n<previous-summary>\nold\n"));
        assert!(bounded.prompt.ends_with(UPDATE_SUMMARIZATION_PROMPT));
        assert!(bounded.conversation_bytes() < request.conversation_bytes());

        // The fixed parts alone over the limit leave only the marker.
        let mut hopeless = request;
        hopeless.bound_conversation(10);
        assert!(hopeless
            .prompt
            .starts_with("<conversation>\n\n...[conversation bounded]...\n\n</conversation>"));
    }

    #[test]
    fn custom_instructions_are_appended_as_additional_focus() {
        let messages = [ProviderMessage::user("x")];
        let first = build_history_summary_request(&messages, 100, Some("auth flow"), None);
        assert!(first.prompt.ends_with(&format!(
            "{SUMMARIZATION_PROMPT}\n\nAdditional focus: auth flow"
        )));
        let update = build_history_summary_request(&messages, 100, Some("auth flow"), Some("p"));
        assert!(update.prompt.ends_with(&format!(
            "{UPDATE_SUMMARIZATION_PROMPT}\n\nAdditional focus: auth flow"
        )));
    }

    #[test]
    fn empty_instructions_and_summaries_count_as_absent() {
        let messages = [ProviderMessage::user("x")];
        let request = build_history_summary_request(&messages, 100, Some(""), Some(""));
        assert_eq!(
            request,
            build_history_summary_request(&messages, 100, None, None)
        );
        assert!(!request.prompt.contains("previous-summary"));
        assert!(!request.prompt.contains("Additional focus"));
    }

    #[test]
    fn prompts_keep_pis_wording() {
        assert!(
            SUMMARIZATION_SYSTEM_PROMPT.starts_with("You are a context summarization assistant.")
        );
        assert!(SUMMARIZATION_SYSTEM_PROMPT.ends_with("ONLY output the structured summary."));
        assert!(
            SUMMARIZATION_PROMPT.starts_with("The messages above are a conversation to summarize.")
        );
        assert!(SUMMARIZATION_PROMPT.contains("## Constraints & Preferences"));
        assert!(SUMMARIZATION_PROMPT.ends_with(
            "Keep each section concise. Preserve exact file paths, function names, and error messages."
        ));
        assert!(UPDATE_SUMMARIZATION_PROMPT.starts_with(
            "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n"
        ));
        assert!(UPDATE_SUMMARIZATION_PROMPT
            .contains("- [x] [Include previously done items AND newly completed items]"));
        assert!(UPDATE_SUMMARIZATION_PROMPT.ends_with(
            "Keep each section concise. Preserve exact file paths, function names, and error messages."
        ));
        assert!(TURN_PREFIX_SUMMARIZATION_PROMPT.starts_with(
            "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.\n\n"
        ));
        assert!(TURN_PREFIX_SUMMARIZATION_PROMPT.ends_with(
            "Only summarize information explicitly present above. Do not infer or recreate later messages."
        ));
        assert_eq!(
            COMPACTION_SUMMARY_PREFIX,
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\n"
        );
        assert_eq!(COMPACTION_SUMMARY_SUFFIX, "\n</summary>");
    }

    // Pi: compaction-summary-reasoning.test.ts
    #[test]
    fn a_split_turn_without_history_skips_the_history_call_and_keeps_the_previous_summary() {
        let messages = vec![ProviderMessage::user("Summarize this.")];
        let preparation = preparation(
            Vec::new(),
            messages,
            true,
            Some("previous checkpoint"),
            2000,
        );
        let requests = preparation.summary_requests(None);

        assert_eq!(requests.history, None);
        let turn_prefix = requests.turn_prefix.expect("turn prefix call");
        assert!(turn_prefix
            .prompt
            .contains("# Conversation\n[User]: Summarize this."));
        assert!(turn_prefix.prompt.contains(
            "# Instructions\nThe messages above are earlier context from an ongoing conversation."
        ));
        let summary = preparation
            .assemble_summary(None, Some("turn text"))
            .unwrap();
        assert_eq!(
            summary,
            "previous checkpoint\n\n---\n\n**Turn Context (split turn):**\n\nturn text"
        );
    }

    #[test]
    fn a_split_turn_without_any_history_uses_the_placeholder() {
        let preparation = preparation(
            Vec::new(),
            vec![ProviderMessage::user("q")],
            true,
            None,
            2000,
        );
        assert_eq!(
            preparation.assemble_summary(None, Some("turn")).unwrap(),
            "No prior history.\n\n---\n\n**Turn Context (split turn):**\n\nturn"
        );
    }

    #[test]
    fn a_split_turn_merges_history_and_turn_prefix_with_pis_separator() {
        let preparation = preparation(
            vec![ProviderMessage::user("old")],
            vec![ProviderMessage::user("q")],
            true,
            None,
            2000,
        );
        let requests = preparation.summary_requests(Some("focus"));
        let history = requests.history.expect("history call");
        assert!(history.prompt.contains("[User]: old"));
        assert!(history.prompt.ends_with("Additional focus: focus"));
        // Custom instructions apply to the history call only.
        assert!(!requests
            .turn_prefix
            .unwrap()
            .prompt
            .contains("Additional focus"));
        assert_eq!(
            preparation
                .assemble_summary(Some("## Goal\nhistory"), Some("## Original Request\nturn"))
                .unwrap(),
            "## Goal\nhistory\n\n---\n\n**Turn Context (split turn):**\n\n## Original Request\nturn"
        );
        assert_eq!(
            merge_split_turn_summary("a", "b"),
            "a\n\n---\n\n**Turn Context (split turn):**\n\nb"
        );
    }

    #[test]
    fn a_plain_compaction_has_one_history_call_and_needs_its_text() {
        let preparation = preparation(
            vec![ProviderMessage::user("old")],
            Vec::new(),
            false,
            None,
            2000,
        );
        let requests = preparation.summary_requests(None);
        assert!(requests.history.is_some());
        assert_eq!(requests.turn_prefix, None);
        assert_eq!(
            preparation.assemble_summary(Some("text"), None).unwrap(),
            "text"
        );
        assert!(preparation.assemble_summary(None, None).is_err());
    }

    #[test]
    fn a_split_turn_needs_both_answers() {
        let preparation = preparation(
            vec![ProviderMessage::user("old")],
            vec![ProviderMessage::user("q")],
            true,
            None,
            2000,
        );
        assert!(preparation.assemble_summary(None, Some("t")).is_err());
        assert!(preparation.assemble_summary(Some("h"), None).is_err());
    }

    #[test]
    fn a_split_flag_without_prefix_messages_is_a_plain_compaction() {
        let preparation = preparation(
            vec![ProviderMessage::user("old")],
            Vec::new(),
            true,
            None,
            2000,
        );
        let requests = preparation.summary_requests(None);
        assert!(requests.history.is_some() && requests.turn_prefix.is_none());
        assert_eq!(preparation.assemble_summary(Some("h"), None).unwrap(), "h");
    }

    #[test]
    fn output_caps_are_a_share_of_the_reserve() {
        let messages = [ProviderMessage::user("x")];
        assert_eq!(
            build_history_summary_request(&messages, 16_384, None, None).max_output_tokens,
            13_107
        );
        assert_eq!(
            build_turn_prefix_summary_request(&messages, 16_384).max_output_tokens,
            8_192
        );
        // Pi's clamp to the model output cap happens on the wire request
        // (tests/provider_http.rs), not here.
        assert_eq!(
            build_history_summary_request(&messages, 1_000, None, None).max_output_tokens,
            800
        );
    }

    #[test]
    fn summary_messages_round_trip() {
        let message = compaction_summary_message("## Goal\nx");
        assert_eq!(message.role, "user");
        assert_eq!(
            message.content,
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\n## Goal\nx\n</summary>"
        );
        assert_eq!(compaction_summary_text(&message), Some("## Goal\nx"));
        assert_eq!(
            compaction_summary_text(&ProviderMessage::user("hello")),
            None
        );
        let mut assistant = message.clone();
        assistant.role = "assistant".into();
        assert_eq!(compaction_summary_text(&assistant), None);
        assert_eq!(
            last_compaction_summary_index(&[
                ProviderMessage::user("a"),
                message,
                ProviderMessage::user("b")
            ]),
            Some(1)
        );
    }

    #[test]
    fn a_kept_duplicate_pointer_whose_original_was_summarized_away_is_rewritten() {
        let pointer = "[duplicate read result omitted; identical output already in context]";
        let messages = vec![
            ProviderMessage::user("task"),
            ProviderMessage::tool("read", "c1", "full file output"),
            ProviderMessage::assistant("noted", Vec::new()),
            ProviderMessage::user("again"),
            ProviderMessage::tool("read", "c2", pointer),
            ProviderMessage::assistant("done", Vec::new()),
        ];
        // The cut falls between the original result and its pointer.
        let compacted = apply_compaction(&messages, 3, "S");
        let rewritten = &compacted[2];
        assert_eq!(rewritten.role, "tool");
        assert!(rewritten.content.starts_with(DUPLICATE_POINTER_PREFIX));
        assert!(rewritten.content.contains("compacted away"));
        assert_ne!(rewritten.content, pointer);
        // The journal still recorded the original pointer.
        assert_eq!(rewritten.recorded_content.as_deref(), Some(pointer));
        // Compacting again leaves the note as it is.
        let again = apply_compaction(&compacted, 1, "S2");
        assert_eq!(again[2].content, rewritten.content);
    }

    #[test]
    fn a_duplicate_pointer_keeps_its_claim_while_a_full_output_is_still_kept() {
        let pointer = "[duplicate read result omitted; identical output already in context]";
        let messages = vec![
            ProviderMessage::user("task"),
            ProviderMessage::tool("read", "c1", "full file output"),
            ProviderMessage::tool("read", "c2", pointer),
            ProviderMessage::tool("grep", "c3", pointer.replace("read", "grep")),
        ];
        let compacted = apply_compaction(&messages, 1, "S");
        // The original `read` is kept, so its pointer stays; the `grep` one
        // has no kept original.
        assert_eq!(compacted[2].content, pointer);
        assert_eq!(compacted[2].recorded_content, None);
        assert!(compacted[3].content.contains("compacted away"));
    }

    #[test]
    fn apply_compaction_keeps_system_messages_and_the_kept_tail() {
        let mut system = ProviderMessage::user("rules");
        system.role = "system".into();
        let messages = [
            system.clone(),
            ProviderMessage::user("old"),
            ProviderMessage::assistant("old answer", Vec::new()),
            ProviderMessage::user("kept"),
            ProviderMessage::assistant("kept answer", Vec::new()),
        ];
        let compacted = apply_compaction(&messages, 3, "S");
        assert_eq!(
            compacted,
            vec![
                system,
                compaction_summary_message("S"),
                messages[3].clone(),
                messages[4].clone()
            ]
        );
    }

    #[test]
    fn compacting_twice_replaces_the_previous_summary() {
        let mut messages = vec![ProviderMessage::user("task")];
        for index in 0..8 {
            messages.push(ProviderMessage::assistant(
                format!("answer {index} {}", "w".repeat(80)),
                Vec::new(),
            ));
            messages.push(ProviderMessage::user(format!(
                "question {index} {}",
                "w".repeat(80)
            )));
        }
        let settings = CompactionSettings {
            keep_recent_tokens: 60,
            ..DEFAULT_COMPACTION_SETTINGS
        };
        let first = prepare_compaction(&messages, &settings, ContextUsage::default()).unwrap();
        let compacted = apply_compaction(&messages, first.first_kept_index, "first");
        assert_eq!(compaction_summary_text(&compacted[0]), Some("first"));

        let mut grown = compacted;
        for index in 0..8 {
            grown.push(ProviderMessage::assistant(
                format!("later answer {index} {}", "z".repeat(80)),
                Vec::new(),
            ));
            grown.push(ProviderMessage::user(format!(
                "later question {index} {}",
                "z".repeat(80)
            )));
        }
        let second = prepare_compaction(&grown, &settings, ContextUsage::default()).unwrap();
        assert_eq!(second.previous_summary.as_deref(), Some("first"));
        assert!(second
            .messages_to_summarize
            .iter()
            .all(|message| compaction_summary_text(message).is_none()));
        let recompacted = apply_compaction(&grown, second.first_kept_index, "second");
        let summaries = recompacted
            .iter()
            .filter(|message| compaction_summary_text(message).is_some())
            .count();
        assert_eq!(summaries, 1);
        assert_eq!(compaction_summary_text(&recompacted[0]), Some("second"));
    }
}
