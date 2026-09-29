use super::*;

#[derive(Clone, Debug, Default)]
pub(super) struct BufferedToolCall {
    index: Option<u32>,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    legacy_seen: bool,
    input_delta_seen: bool,
    malformed: bool,
}

impl BufferedToolCall {
    fn new(index: Option<u32>, id: Option<String>, name: Option<String>) -> Self {
        Self {
            index,
            id,
            name,
            ..Self::default()
        }
    }
}

/// How a provider's tool calls arrive on the wire, computed once from its kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Wire {
    Chat,
    Responses,
    Anthropic,
    /// Kinds that buffer no streamed tool calls.
    Other,
}

impl Wire {
    fn of(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::OpenAiCompatible => Self::Chat,
            ProviderKind::OpenAiCodex => Self::Responses,
            ProviderKind::Anthropic => Self::Anthropic,
            _ => Self::Other,
        }
    }

    fn is_openai(self) -> bool {
        matches!(self, Self::Chat | Self::Responses)
    }
}

/// Converts the raw provider event stream into the runtime ledger, redacting
/// secrets on the fly and buffering streamed tool calls per adapter style.
pub(crate) struct ProviderStreamNormalizer {
    wire: Wire,
    next_seq: u64,
    openai_calls: Vec<BufferedToolCall>,
    anthropic_calls: Vec<BufferedToolCall>,
    standalone_calls: Vec<BufferedToolCall>,
    sensitive_values: Vec<String>,
    text_pending: String,
    reasoning_pending: String,
    reasoning_open: bool,
    reasoning_classification: Option<crate::ReasoningClassification>,
    responses_reasoning: Vec<crate::provider::ResponsesReasoning>,
    chat_reasoning: Option<crate::provider::ChatReasoning>,
    stopped: bool,
    pub(super) error: Option<ProviderError>,
    stop_reason: Option<String>,
    terminal_usage_seen: bool,
    input_usage_complete_seen: bool,
    output_usage_complete_seen: bool,
    preparing_tool_announced: bool,
}

impl ProviderStreamNormalizer {
    pub(crate) fn new(kind: ProviderKind, next_seq: u64, sensitive_values: Vec<String>) -> Self {
        Self {
            wire: Wire::of(kind),
            next_seq,
            openai_calls: Vec::new(),
            anthropic_calls: Vec::new(),
            standalone_calls: Vec::new(),
            sensitive_values,
            text_pending: String::new(),
            reasoning_pending: String::new(),
            reasoning_open: false,
            reasoning_classification: None,
            responses_reasoning: Vec::new(),
            chat_reasoning: None,
            stopped: false,
            error: None,
            stop_reason: None,
            terminal_usage_seen: false,
            input_usage_complete_seen: false,
            output_usage_complete_seen: false,
            preparing_tool_announced: false,
        }
    }

    pub(crate) fn with_reasoning_classification(
        mut self,
        classification: Option<crate::ReasoningClassification>,
    ) -> Self {
        self.reasoning_classification = classification;
        self
    }

    pub(super) fn next_seq(&self) -> u64 {
        self.next_seq
    }

    fn prune_unused_tool_slots(&mut self) {
        prune_unused_buffered_slots(&mut self.openai_calls);
        prune_unused_buffered_slots(&mut self.standalone_calls);
    }

    fn take_open_anthropic_calls(&mut self) {
        self.standalone_calls.append(&mut self.anthropic_calls);
    }

    pub(super) fn argument_repair_note(&mut self) -> Option<String> {
        if self.error.is_some() || !self.stopped {
            return None;
        }
        self.take_open_anthropic_calls();
        self.prune_unused_tool_slots();
        let reason = self.stop_reason.as_deref()?;
        if classify_provider_stop_reason(reason, &self.sensitive_values).ok()?
            != ProviderTurnStop::Normal
            || !(stop_requires_tool_calls(reason)
                || (self.wire == Wire::Anthropic && !self.standalone_calls.is_empty())
                || (self.wire == Wire::Responses && reason.eq_ignore_ascii_case("completed")))
        {
            return None;
        }
        let mut identities = std::collections::BTreeSet::new();
        let mut notes = Vec::new();
        for call in self.openai_calls.iter().chain(&self.standalone_calls) {
            let id = call.id.as_deref().filter(|id| !id.trim().is_empty())?;
            let name = call
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())?;
            if call.malformed || !identities.insert(id) {
                return None;
            }
            let Some(issue) = object_arguments(&call.arguments).1.issue() else {
                continue;
            };
            notes.push(format!(
                "call {} ({}): {}; received {}",
                truncate_result(&redact_values(&self.sensitive_values, id), 128),
                truncate_result(&redact_values(&self.sensitive_values, name), 128),
                issue,
                truncate_result(&redact_values(&self.sensitive_values, &call.arguments), 512)
            ));
        }
        (!notes.is_empty()).then(|| {
            truncate_result(
                &redact_values(&self.sensitive_values, &notes.join("\n")),
                4096,
            )
        })
    }

    pub(crate) fn push(&mut self, app: &mut AppHandle, event: ProviderEvent) {
        // A protocol failure blocks content and tools, not observed usage.
        // Keep validating accounting and preserve the original failure.
        if self.error.is_some()
            && !matches!(
                &event,
                ProviderEvent::Usage { .. }
                    | ProviderEvent::UsagePartial { .. }
                    | ProviderEvent::UsageBreakdown { .. }
            )
        {
            return;
        }
        if let Err(error) = self.push_inner(app, event) {
            self.error.get_or_insert(error);
        }
    }

    pub(super) fn finish(
        mut self,
        app: &mut AppHandle,
    ) -> Result<ProviderTurnResult, ProviderError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !self.stopped {
            return Err(ProviderError::InvalidResponse {
                message: NO_STOP_REASON_MESSAGE.into(),
            });
        }
        // A call still open at the stop is promoted, not rejected: the
        // argument gate below refuses truncated JSON, and an identified call
        // with invalid JSON stays repairable by the loop.
        self.take_open_anthropic_calls();
        self.flush_text(app)?;
        let raw_stop_reason =
            self.stop_reason
                .clone()
                .ok_or_else(|| ProviderError::InvalidResponse {
                    message: NO_STOP_REASON_MESSAGE.into(),
                })?;
        let stop = classify_provider_stop_reason(&raw_stop_reason, &self.sensitive_values)?;
        let codex_completed_with_calls = self.wire == Wire::Responses
            && raw_stop_reason.trim().eq_ignore_ascii_case("completed")
            && (!self.openai_calls.is_empty() || !self.standalone_calls.is_empty());
        let anthropic_completed_with_calls =
            self.wire == Wire::Anthropic && !self.standalone_calls.is_empty();
        if stop == ProviderTurnStop::Normal
            && (stop_requires_tool_calls(&raw_stop_reason)
                || codex_completed_with_calls
                || anthropic_completed_with_calls)
        {
            self.prune_unused_tool_slots();
            let mut calls = std::mem::take(&mut self.openai_calls);
            calls.sort_by_key(|call| call.index.unwrap_or(u32::MAX));
            calls.extend(std::mem::take(&mut self.standalone_calls));
            if calls.is_empty() {
                return Err(ProviderError::InvalidResponse {
                    message: "provider required tool execution but emitted no complete tool call"
                        .into(),
                });
            }
            let mut call_ids = std::collections::BTreeSet::new();
            let mut normalized = Vec::with_capacity(calls.len());
            for call in &calls {
                normalized.push(validate_buffered_call(call)?.into_owned());
                if sensitive_tool_arguments(&call.arguments, &self.sensitive_values)
                    || [
                        call.id.as_deref().unwrap_or(""),
                        call.name.as_deref().unwrap_or(""),
                    ]
                    .iter()
                    .any(|value| {
                        self.sensitive_values
                            .iter()
                            .any(|secret| !secret.is_empty() && value.contains(secret))
                    })
                {
                    return Err(ProviderError::InvalidResponse {
                        message: "tool call contains registered sensitive material; use a configured credential reference".into(),
                    });
                }
                if call.id.as_deref().is_some_and(|id| !call_ids.insert(id)) {
                    return Err(ProviderError::MalformedToolCall);
                }
            }
            for (call, arguments) in calls.into_iter().zip(normalized) {
                publish_buffered_call(app, &mut self.next_seq, call, arguments)?;
            }
        } else {
            self.openai_calls.clear();
            self.standalone_calls.clear();
        }
        push_runtime_event(
            app,
            &mut self.next_seq,
            crate::EventKind::AssistantEnded {
                reason: redact_values(&self.sensitive_values, &raw_stop_reason),
            },
        )?;
        // Tool side effects of an aborted turn must not run: only a normal
        // stop lets the loop execute the buffered tool calls.
        let blocks_tools = !matches!(stop, ProviderTurnStop::Normal);
        Ok(ProviderTurnResult {
            next_seq: self.next_seq,
            blocks_tools,
            stop,
            responses_reasoning: self.responses_reasoning,
            chat_reasoning: self.chat_reasoning,
        })
    }

    /// Finish used by the provider-facing one-shot runner: returns the next
    /// sequence, and an error unless the provider stopped normally.
    pub(crate) fn finish_free(self, app: &mut AppHandle) -> Result<u64, ProviderError> {
        if self.error.is_some() {
            return self.finish(app).map(|turn| turn.next_seq);
        }
        let raw_stop_reason =
            self.stop_reason
                .as_deref()
                .ok_or_else(|| ProviderError::InvalidResponse {
                    message: NO_STOP_REASON_MESSAGE.into(),
                })?;
        if classify_provider_stop_reason(raw_stop_reason, &self.sensitive_values)?
            != ProviderTurnStop::Normal
        {
            return Err(ProviderError::InvalidResponse {
                message: "provider did not complete successfully".into(),
            });
        }
        let turn = self.finish(app)?;
        Ok(turn.next_seq)
    }

    fn push_inner(
        &mut self,
        app: &mut AppHandle,
        event: ProviderEvent,
    ) -> Result<(), ProviderError> {
        if self.stopped
            && !matches!(
                &event,
                ProviderEvent::Usage { .. }
                    | ProviderEvent::UsagePartial { .. }
                    | ProviderEvent::UsageBreakdown { .. }
                    | ProviderEvent::Stopped { .. }
            )
        {
            return Err(ProviderError::InvalidResponse {
                message: "provider emitted events after stop".into(),
            });
        }
        if event_closes_reasoning(&event) {
            self.close_reasoning(app)?;
        }
        match event {
            ProviderEvent::Phase { phase, elapsed_ms } => push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::ProviderPhase {
                    phase,
                    elapsed_ms,
                    detail: None,
                },
            ),
            ProviderEvent::ToolCallProgress { name, bytes } => {
                self.on_tool_progress(app, name.as_deref(), bytes)
            }
            ProviderEvent::ResponsesReasoning(state) => {
                self.responses_reasoning.push(state);
                Ok(())
            }
            ProviderEvent::ChatReasoning(state) => self.on_chat_reasoning(state),
            ProviderEvent::TextDelta(text) => self.emit_chunk(app, false, &text, false),
            ProviderEvent::ReasoningDelta(text) => {
                self.open_reasoning(app)?;
                self.emit_chunk(app, true, &text, false)
            }
            ProviderEvent::ReasoningStarted => self.open_reasoning(app),
            ProviderEvent::ReasoningEnded => self.close_reasoning(app),
            ProviderEvent::UsageBreakdown { usage } => push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::UsageBreakdown { usage },
            ),
            ProviderEvent::ResponseCacheHit => {
                push_runtime_event(app, &mut self.next_seq, crate::EventKind::ResponseCacheHit)
            }
            ProviderEvent::Usage {
                input_tokens,
                output_tokens,
            } => self.on_usage(app, input_tokens, output_tokens),
            ProviderEvent::UsagePartial {
                input_tokens,
                output_tokens,
                input_complete,
                output_complete,
            } => self.on_usage_partial(
                app,
                input_tokens,
                output_tokens,
                input_complete,
                output_complete,
            ),
            ProviderEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } if self.wire.is_openai() => self.on_openai_delta(app, index, id, name, arguments),
            ProviderEvent::ToolCallComplete {
                index,
                id,
                name,
                arguments,
            } if self.wire == Wire::Responses => {
                self.on_codex_complete(index, id, name, &arguments)
            }
            ProviderEvent::ToolCallStart { index, id, name } if self.wire == Wire::Anthropic => {
                self.on_anthropic_start(app, index, id, name)
            }
            ProviderEvent::ToolCallInputDelta {
                index,
                partial_json,
            } if self.wire == Wire::Anthropic => self.on_anthropic_input(index, &partial_json),
            ProviderEvent::ContentBlockStop { index } if self.wire == Wire::Anthropic => {
                self.on_anthropic_stop(index);
                Ok(())
            }
            ProviderEvent::ToolCall { name, arguments } => self.on_legacy_call(name, arguments),
            ProviderEvent::Stopped { reason } => self.on_stopped(app, reason),
            ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::ToolCallComplete { .. }
            | ProviderEvent::ToolCallStart { .. }
            | ProviderEvent::ToolCallInputDelta { .. }
            | ProviderEvent::ContentBlockStop { .. } => Err(ProviderError::MalformedToolCall),
        }
    }

    /// The call is still being written: show it as the preparing phase with
    /// its size. Transient (not journaled, coalescible), the same event the
    /// final, held tool call later announces without a size.
    fn on_tool_progress(
        &mut self,
        app: &mut AppHandle,
        name: Option<&str>,
        bytes: u64,
    ) -> Result<(), ProviderError> {
        push_runtime_transient_event(
            app,
            &mut self.next_seq,
            crate::EventKind::ProviderPhase {
                phase: crate::provider::ProviderPhase::PreparingTool,
                elapsed_ms: 0,
                detail: Some(tool_progress_detail(
                    name.map(|name| redact_values(&self.sensitive_values, name))
                        .as_deref(),
                    bytes,
                )),
            },
        )
    }

    fn announce_preparing(&mut self, app: &mut AppHandle, name: &str) -> Result<(), ProviderError> {
        push_runtime_event(
            app,
            &mut self.next_seq,
            crate::EventKind::ProviderPhase {
                phase: crate::provider::ProviderPhase::PreparingTool,
                elapsed_ms: 0,
                detail: Some(redact_values(&self.sensitive_values, name)),
            },
        )
    }

    fn on_chat_reasoning(
        &mut self,
        state: crate::provider::ChatReasoning,
    ) -> Result<(), ProviderError> {
        if let Some(previous) = self.chat_reasoning.as_mut() {
            if previous.scope_id != state.scope_id || previous.model != state.model {
                return Err(ProviderError::InvalidResponse {
                    message: "Chat reasoning scope changed during a response".into(),
                });
            }
            previous.content.push_str(&state.content);
            previous.details.extend(state.details);
        } else {
            self.chat_reasoning = Some(state);
        }
        Ok(())
    }

    fn on_usage(
        &mut self,
        app: &mut AppHandle,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<(), ProviderError> {
        if self.terminal_usage_seen {
            return Err(ProviderError::InvalidResponse {
                message: "provider emitted terminal usage more than once".into(),
            });
        }
        self.terminal_usage_seen = true;
        push_runtime_event(
            app,
            &mut self.next_seq,
            crate::EventKind::Usage {
                input_tokens,
                output_tokens,
            },
        )
    }

    fn on_usage_partial(
        &mut self,
        app: &mut AppHandle,
        input_tokens: u64,
        output_tokens: u64,
        input_complete: bool,
        output_complete: bool,
    ) -> Result<(), ProviderError> {
        if (input_complete && self.input_usage_complete_seen)
            || (output_complete && self.output_usage_complete_seen)
        {
            return Err(ProviderError::InvalidResponse {
                message: "provider repeated a complete usage component".into(),
            });
        }
        self.input_usage_complete_seen |= input_complete;
        self.output_usage_complete_seen |= output_complete;
        push_runtime_event(
            app,
            &mut self.next_seq,
            crate::EventKind::UsagePartial {
                input_tokens,
                output_tokens,
                input_known: input_complete,
                output_known: output_complete,
            },
        )
    }

    fn on_openai_delta(
        &mut self,
        app: &mut AppHandle,
        index: Option<u32>,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    ) -> Result<(), ProviderError> {
        let preparing_name = (!self.preparing_tool_announced)
            .then(|| name.as_deref().map(str::trim))
            .flatten()
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        append_openai_delta(&mut self.openai_calls, index, id, name, arguments);
        if let Some(name) = preparing_name {
            self.preparing_tool_announced = true;
            self.announce_preparing(app, &name)?;
        }
        Ok(())
    }

    fn on_codex_complete(
        &mut self,
        index: u32,
        id: String,
        name: String,
        arguments: &str,
    ) -> Result<(), ProviderError> {
        append_openai_delta(
            &mut self.openai_calls,
            Some(index),
            Some(id),
            Some(name.clone()),
            String::new(),
        );
        let call = self
            .openai_calls
            .iter_mut()
            .find(|call| call.index == Some(index))
            .ok_or(ProviderError::MalformedToolCall)?;
        if call.malformed {
            return Err(ProviderError::MalformedToolCall);
        }
        // The final item is a snapshot of this identified call, not a
        // second call or a fragment to append. Never match by content.
        attach_legacy_call(std::slice::from_mut(call), &name, arguments)?;
        // Arguments stay unvalidated here so an identified call with
        // invalid JSON can still be repaired by the loop.
        Ok(())
    }

    fn on_anthropic_start(
        &mut self,
        app: &mut AppHandle,
        index: u32,
        id: String,
        name: String,
    ) -> Result<(), ProviderError> {
        start_anthropic_call(&mut self.anthropic_calls, index, id, name.clone())?;
        self.announce_preparing(app, &name)
    }

    fn on_anthropic_input(&mut self, index: u32, partial_json: &str) -> Result<(), ProviderError> {
        let Some(call) = self
            .anthropic_calls
            .iter_mut()
            .find(|call| call.index == Some(index))
        else {
            return Err(ProviderError::MalformedToolCall);
        };
        if call.legacy_seen {
            call.arguments.clear();
            call.legacy_seen = false;
        }
        call.input_delta_seen = true;
        call.arguments.push_str(partial_json);
        Ok(())
    }

    fn on_anthropic_stop(&mut self, index: u32) {
        let Some(position) = self
            .anthropic_calls
            .iter()
            .position(|call| call.index == Some(index))
        else {
            return;
        };
        // Buffer the whole batch until the message's terminal reason
        // is known. A malformed sibling must prevent every execution.
        self.standalone_calls
            .push(self.anthropic_calls.remove(position));
    }

    fn on_legacy_call(&mut self, name: String, arguments: String) -> Result<(), ProviderError> {
        if self.wire == Wire::Anthropic
            && attach_legacy_call(&mut self.anthropic_calls, &name, &arguments)?
        {
            return Ok(());
        }
        if self.wire.is_openai() && attach_legacy_call(&mut self.openai_calls, &name, &arguments)? {
            return Ok(());
        }
        validate_tool_arguments(&name, &arguments)?;
        self.standalone_calls.push(BufferedToolCall {
            name: Some(name),
            arguments,
            legacy_seen: true,
            ..BufferedToolCall::default()
        });
        Ok(())
    }

    fn on_stopped(&mut self, app: &mut AppHandle, reason: String) -> Result<(), ProviderError> {
        if let Some(existing) = &self.stop_reason {
            if existing.trim().eq_ignore_ascii_case(reason.trim()) {
                return Ok(());
            }
            return Err(ProviderError::InvalidResponse {
                message: "provider emitted more than one stop reason".into(),
            });
        }
        self.stop_reason = Some(reason);
        self.flush_text(app)?;
        self.stopped = true;
        Ok(())
    }

    /// Releases everything still held, including a tail that could start a secret.
    /// Only for the end of the stream: a channel switch keeps the tail (see below).
    pub(super) fn flush_text(&mut self, app: &mut AppHandle) -> Result<(), ProviderError> {
        self.flush_assistant(app, true)?;
        self.flush_reasoning(app)
    }

    /// `force` false keeps a tail that is a prefix of a registered secret, so a secret
    /// split by a text/reasoning switch is still redacted once the channel resumes.
    fn flush_assistant(&mut self, app: &mut AppHandle, force: bool) -> Result<(), ProviderError> {
        self.emit_chunk(app, false, "", force)
    }

    /// Always releases the whole reasoning tail: a reasoning delta must fall inside
    /// its `ThinkingStarted`/`ThinkingEnded` lifecycle, so it cannot be held past
    /// `close_reasoning`. Only the text channel, which has no lifecycle, holds a
    /// secret-prefix tail across a channel switch.
    fn flush_reasoning(&mut self, app: &mut AppHandle) -> Result<(), ProviderError> {
        self.emit_chunk(app, true, "", true)
    }

    /// Appends `delta` to one channel's pending text and publishes what is
    /// safe to release (`flush` releases a secret-prefix tail too).
    fn emit_chunk(
        &mut self,
        app: &mut AppHandle,
        reasoning: bool,
        delta: &str,
        flush: bool,
    ) -> Result<(), ProviderError> {
        let pending = if reasoning {
            &mut self.reasoning_pending
        } else {
            &mut self.text_pending
        };
        let text = take_redacted_stream_chunk(pending, delta, &self.sensitive_values, flush);
        if text.is_empty() {
            return Ok(());
        }
        let kind = if reasoning {
            crate::EventKind::ReasoningDelta { text }
        } else {
            crate::EventKind::AssistantTextDelta { text }
        };
        push_runtime_event(app, &mut self.next_seq, kind)
    }

    fn open_reasoning(&mut self, app: &mut AppHandle) -> Result<(), ProviderError> {
        self.flush_assistant(app, false)?;
        if self.reasoning_open {
            return Ok(());
        }
        if let Some(classification) = self.reasoning_classification.take() {
            push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::ReasoningClassification { classification },
            )?;
        }
        push_runtime_event(app, &mut self.next_seq, crate::EventKind::ThinkingStarted)?;
        self.reasoning_open = true;
        Ok(())
    }

    fn close_reasoning(&mut self, app: &mut AppHandle) -> Result<(), ProviderError> {
        self.flush_reasoning(app)?;
        if !std::mem::take(&mut self.reasoning_open) {
            return Ok(());
        }
        push_runtime_event(app, &mut self.next_seq, crate::EventKind::ThinkingEnded)
    }
}

pub(super) fn stop_requires_tool_calls(reason: &str) -> bool {
    matches!(
        reason.trim().to_ascii_lowercase().as_str(),
        "tool_calls" | "function_call" | "tool_use"
    )
}

pub(crate) fn take_redacted_stream_chunk(
    pending: &mut String,
    delta: &str,
    sensitive_values: &[String],
    flush: bool,
) -> String {
    if sensitive_values.is_empty() {
        pending.push_str(delta);
        return std::mem::take(pending);
    }
    if pending.is_empty() {
        // Nothing carried over: split the delta itself and keep only its tail.
        let split_at = if flush {
            delta.len()
        } else {
            safe_stream_split(delta, sensitive_values)
        };
        pending.push_str(&delta[split_at..]);
        return redact_values(sensitive_values, &delta[..split_at]);
    }
    pending.push_str(delta);
    let split_at = if flush {
        pending.len()
    } else {
        safe_stream_split(pending, sensitive_values)
    };
    let ready = redact_values(sensitive_values, &pending[..split_at]);
    pending.drain(..split_at);
    ready
}

pub(super) fn safe_stream_split(input: &str, sensitive_values: &[String]) -> usize {
    let held_bytes = sensitive_values
        .iter()
        .flat_map(|value| {
            value
                .char_indices()
                .skip(1)
                .take_while(|(index, _)| *index <= input.len())
                .map(move |(index, _)| &value[..index])
        })
        .filter(|prefix| input.ends_with(prefix))
        .map(str::len)
        .max()
        .unwrap_or(0);
    let mut split_at = input.len().saturating_sub(held_bytes);
    // Nothing is held: a match cannot straddle the end of the input.
    if held_bytes == 0 {
        return split_at;
    }
    loop {
        let adjusted = sensitive_values
            .iter()
            .flat_map(|value| {
                input
                    .match_indices(value)
                    .map(move |(start, _)| (start, start + value.len()))
            })
            .filter(|(start, end)| *start < split_at && split_at < *end)
            .map(|(start, _)| start)
            .min()
            .unwrap_or(split_at);
        if adjusted == split_at {
            return split_at;
        }
        split_at = adjusted;
    }
}

pub(super) fn event_closes_reasoning(event: &ProviderEvent) -> bool {
    match event {
        ProviderEvent::TextDelta(text) => !text.trim().is_empty(),
        ProviderEvent::ToolCallDelta { name, .. } => {
            name.as_deref().is_some_and(|name| !name.trim().is_empty())
        }
        ProviderEvent::ToolCallStart { .. }
        | ProviderEvent::ToolCallInputDelta { .. }
        | ProviderEvent::ToolCallComplete { .. }
        | ProviderEvent::ToolCall { .. }
        | ProviderEvent::ContentBlockStop { .. }
        | ProviderEvent::Stopped { .. } => true,
        _ => false,
    }
}

pub(super) fn buffered_call_has_name(call: &BufferedToolCall) -> bool {
    call.name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
}

pub(super) fn buffered_call_is_complete(call: &BufferedToolCall) -> bool {
    if call.malformed {
        return false;
    }
    buffered_call_has_name(call) && object_arguments(&call.arguments).1.is_object()
}

pub(super) fn prune_unused_buffered_slots(calls: &mut Vec<BufferedToolCall>) {
    if !calls.iter().any(buffered_call_has_name) {
        return;
    }
    let complete = calls
        .iter()
        .map(buffered_call_is_complete)
        .collect::<Vec<_>>();
    let has_complete = complete.contains(&true);
    let mut complete = complete.into_iter();
    calls.retain(|call| {
        if complete.next() == Some(true) || call.malformed {
            return true;
        }
        if has_complete
            && call.arguments.trim().is_empty()
            && call.id.as_deref().is_none_or(|id| id.trim().is_empty())
        {
            return false;
        }
        buffered_call_has_name(call) || !call.arguments.trim().is_empty()
    });
}

/// Share the executable call assembler with the transport's secret gate so
/// index/id fallback and repeated headers cannot change redaction semantics.
pub(crate) fn tool_events_contain_sensitive_values(
    events: &[ProviderEvent],
    secrets: &[String],
) -> bool {
    if secrets.is_empty() {
        return false;
    }
    let sensitive = |value: &str| {
        secrets
            .iter()
            .any(|secret| !secret.is_empty() && value.contains(secret))
    };
    let mut calls = Vec::new();
    for event in events {
        match event {
            ProviderEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => append_openai_delta(
                &mut calls,
                *index,
                id.clone(),
                name.clone(),
                arguments.clone(),
            ),
            ProviderEvent::ToolCallStart { index, id, name } => append_openai_delta(
                &mut calls,
                Some(*index),
                Some(id.clone()),
                Some(name.clone()),
                String::new(),
            ),
            ProviderEvent::ToolCallInputDelta {
                index,
                partial_json,
            } => append_openai_delta(&mut calls, Some(*index), None, None, partial_json.clone()),
            ProviderEvent::ToolCallComplete {
                id,
                name,
                arguments,
                ..
            } if sensitive(id)
                || sensitive(name)
                || sensitive_tool_arguments(arguments, secrets) =>
            {
                return true;
            }
            ProviderEvent::ToolCall { name, arguments }
                if sensitive(name) || sensitive_tool_arguments(arguments, secrets) =>
            {
                return true;
            }
            _ => {}
        }
    }
    calls.iter().any(|call| {
        sensitive_tool_arguments(&call.arguments, secrets)
            || sensitive(call.id.as_deref().unwrap_or(""))
            || sensitive(call.name.as_deref().unwrap_or(""))
    })
}

pub(super) fn sensitive_tool_arguments(arguments: &str, secrets: &[String]) -> bool {
    // `object_arguments` unwraps one JSON-string layer at publish
    // time, so the gate cannot stop at a single decode: a string value that
    // itself parses as JSON is checked at every level the executor can reach.
    // Decoded text strictly shrinks per level and nested JSON quoting grows
    // ~2x outward, so real payloads stay far below this bound.
    const MAX_UNWRAP_DEPTH: u32 = 8;
    let has_secret = |text: &str| {
        secrets
            .iter()
            .any(|secret| !secret.is_empty() && text.contains(secret.as_str()))
    };
    fn contains(value: &Value, has_secret: &impl Fn(&str) -> bool, depth: u32) -> bool {
        match value {
            Value::String(text) => {
                has_secret(text)
                    || (depth > 0
                        && serde_json::from_str::<Value>(text)
                            .is_ok_and(|inner| contains(&inner, has_secret, depth - 1)))
            }
            Value::Array(values) => values
                .iter()
                .any(|value| contains(value, has_secret, depth)),
            Value::Object(values) => values
                .iter()
                .any(|(key, value)| has_secret(key) || contains(value, has_secret, depth)),
            _ => has_secret(&value.to_string()),
        }
    }
    secrets.iter().any(|secret| !secret.is_empty())
        && (has_secret(arguments)
            || serde_json::from_str::<Value>(arguments)
                .is_ok_and(|value| contains(&value, &has_secret, MAX_UNWRAP_DEPTH)))
}

pub(super) fn push_malformed(
    calls: &mut Vec<BufferedToolCall>,
    index: Option<u32>,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
) {
    let mut call = BufferedToolCall::new(index, id, name);
    call.arguments = arguments;
    call.malformed = true;
    calls.push(call);
}

/// Sets an empty slot, or reports a conflict when both sides differ.
pub(super) fn merge_field<T: PartialEq>(slot: &mut Option<T>, incoming: Option<T>) -> bool {
    match slot {
        Some(existing) => incoming.is_some_and(|incoming| *existing != incoming),
        None => {
            *slot = incoming;
            false
        }
    }
}

pub(super) fn append_openai_delta(
    calls: &mut Vec<BufferedToolCall>,
    index: Option<u32>,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
) {
    let id = id.filter(|id| !id.is_empty());
    let name = name.filter(|name| !name.trim().is_empty());
    if index.is_none() && id.is_none() && name.is_none() && arguments.is_empty() {
        push_malformed(calls, None, None, None, String::new());
        return;
    }
    let by_index = index.and_then(|value| calls.iter().position(|call| call.index == Some(value)));
    let by_id = id.as_ref().and_then(|value| {
        calls
            .iter()
            .position(|call| call.id.as_ref() == Some(value))
    });
    if let (Some(index_position), Some(id_position)) = (by_index, by_id) {
        if index_position != id_position {
            push_malformed(calls, index, id, name, arguments);
            return;
        }
    }
    let position = if let Some(position) = by_index.or(by_id) {
        position
    } else if index.is_none() && id.is_none() {
        let candidates = calls
            .iter()
            .enumerate()
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [position] => *position,
            [] if name.is_some() => {
                calls.push(BufferedToolCall::new(index, id.clone(), name.clone()));
                calls.len() - 1
            }
            _ => {
                push_malformed(calls, index, id, name, arguments);
                return;
            }
        }
    } else {
        calls.push(BufferedToolCall::new(index, id.clone(), name.clone()));
        calls.len() - 1
    };
    let call = &mut calls[position];
    call.malformed |= merge_field(&mut call.index, index);
    call.malformed |= merge_field(&mut call.id, id);
    call.malformed |= merge_field(&mut call.name, name);
    call.arguments.push_str(&arguments);
}

pub(super) fn start_anthropic_call(
    calls: &mut Vec<BufferedToolCall>,
    index: u32,
    id: String,
    name: String,
) -> Result<(), ProviderError> {
    if id.is_empty()
        || name.is_empty()
        || calls
            .iter()
            .any(|call| call.index == Some(index) || call.id.as_deref() == Some(id.as_str()))
    {
        return Err(ProviderError::MalformedToolCall);
    }
    calls.push(BufferedToolCall::new(Some(index), Some(id), Some(name)));
    Ok(())
}

pub(super) fn attach_legacy_call(
    calls: &mut [BufferedToolCall],
    name: &str,
    arguments: &str,
) -> Result<bool, ProviderError> {
    let matches = calls
        .iter()
        .enumerate()
        .filter(|(_, call)| call.name.as_deref() == Some(name))
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    let exact_matches = matches
        .iter()
        .copied()
        .filter(|position| calls[*position].arguments == arguments)
        .collect::<Vec<_>>();
    let matches = if exact_matches.len() == 1 {
        exact_matches
    } else {
        matches
    };
    let Some(position) = (match matches.as_slice() {
        [] => return Ok(false),
        [position] => Some(*position),
        _ => return Err(ProviderError::MalformedToolCall),
    }) else {
        return Ok(false);
    };
    let call = &mut calls[position];
    if call.input_delta_seen {
        return Ok(true);
    }
    if call.arguments.is_empty() || call.arguments == "{}" {
        arguments.clone_into(&mut call.arguments);
        call.legacy_seen = true;
    } else if call.arguments != arguments {
        if serde_json::from_str::<Value>(&call.arguments).is_err()
            && serde_json::from_str::<Value>(arguments).is_ok()
        {
            arguments.clone_into(&mut call.arguments);
            call.legacy_seen = true;
        } else {
            return Err(ProviderError::MalformedToolCall);
        }
    }
    Ok(true)
}

/// `arguments` is the normalized text `validate_buffered_call` returned for
/// this call; the caller validates every sibling before publishing any.
pub(super) fn publish_buffered_call(
    app: &mut AppHandle,
    next_seq: &mut u64,
    call: BufferedToolCall,
    arguments: String,
) -> Result<(), ProviderError> {
    let Some(name) = call.name.filter(|name| !name.trim().is_empty()) else {
        return Err(ProviderError::MalformedToolCall);
    };
    push_runtime_event(
        app,
        next_seq,
        crate::EventKind::ProviderToolCall {
            id: call.id.unwrap_or_default(),
            name,
            arguments,
        },
    )
}

/// The normalized arguments of a well-formed, named call with object arguments.
pub(super) fn validate_buffered_call(
    call: &BufferedToolCall,
) -> Result<Cow<'_, str>, ProviderError> {
    if call.malformed || call.name.as_deref().unwrap_or_default().trim().is_empty() {
        return Err(ProviderError::MalformedToolCall);
    }
    match object_arguments(&call.arguments) {
        (arguments, ArgumentShape::Object) => Ok(arguments),
        _ => Err(ProviderError::MalformedToolCall),
    }
}

/// `write · 4,2 KB` (or just the size while the name is unknown): the detail of a
/// `PreparingTool` phase that reports a call still being written.
fn tool_progress_detail(name: Option<&str>, bytes: u64) -> String {
    let size = if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0).replace('.', ",")
    };
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("{name} · {size}"),
        None => size,
    }
}
