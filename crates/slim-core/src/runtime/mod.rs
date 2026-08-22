mod app_handle;
mod loop_guard;
mod mode;
mod queue;

use crate::context::{
    build_bounded_summary_prompt, compact_provider_messages, estimate_provider_message_tokens,
    has_compactable_history, ArtifactHandle, ArtifactStore, ContextBudget,
};
use crate::model::AppHandle;
use crate::provider::{
    HttpProviderClient, ProviderAdapter, ProviderError, ProviderEvent, ProviderKind,
    ProviderMessage, ProviderToolCall,
};
use crate::tools::{ToolRegistry, ToolResult};
use serde_json::Value;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub use app_handle::RuntimeHandle;
pub use loop_guard::LoopGuard;
pub use mode::mode_name;
pub use queue::PromptQueue;

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl PartialEq for CancellationToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CancellationToken {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentLoopConfig {
    pub max_turns: usize,
    pub max_tool_calls: usize,
    pub max_result_bytes: usize,
    pub context_window_tokens: u64,
    pub context_reserve_tokens: u64,
    pub context_compaction_enabled: bool,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_turns: 8,
            max_tool_calls: 32,
            max_result_bytes: 64 * 1024,
            context_window_tokens: 32_000,
            context_reserve_tokens: 4_096,
            context_compaction_enabled: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentLoopStop {
    ProviderCompleted,
    TurnLimit,
    ToolLimit,
    RepeatedFailedTool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentLoopResult {
    pub next_seq: u64,
    pub turns: usize,
    pub stop: AgentLoopStop,
    pub tool_results: Vec<ToolResult>,
    pub usage: UsageTotals,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl UsageTotals {
    pub fn add(&mut self, input_tokens: u32, output_tokens: u32) {
        self.input_tokens = self.input_tokens.saturating_add(input_tokens as u64);
        self.output_tokens = self.output_tokens.saturating_add(output_tokens as u64);
    }
}

pub struct Runtime {
    pub app: AppHandle,
    tools: ToolRegistry,
    artifact_store: Option<ArtifactStore>,
    sensitive_values: SensitiveValues,
    cancellation: Option<CancellationToken>,
}

#[derive(Default)]
struct SensitiveValues(Vec<String>);

impl fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("app", &self.app)
            .field("tools", &self.tools)
            .field("artifact_store", &self.artifact_store)
            .finish()
    }
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            app: AppHandle::fake(),
            tools: ToolRegistry::default(),
            artifact_store: None,
            sensitive_values: SensitiveValues::default(),
            cancellation: None,
        }
    }

    pub fn with_artifact_store(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut runtime = Self::new();
        runtime.artifact_store = Some(ArtifactStore::new(root)?);
        Ok(runtime)
    }

    pub fn set_artifact_store(&mut self, store: ArtifactStore) {
        self.artifact_store = Some(store);
    }

    pub fn set_cancellation_token(&mut self, cancellation: CancellationToken) {
        self.cancellation = Some(cancellation);
    }

    pub fn tools_for_mode(&self, mode: crate::OperatingMode) -> Vec<&'static str> {
        self.tools.names_for_mode(mode)
    }

    /// Registers an exact value that must not cross a runtime boundary.
    ///
    /// Runtime diagnostics intentionally omit the storage so they can never
    /// print the secret itself.
    pub fn register_sensitive_value(&mut self, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() && !self.sensitive_values.0.iter().any(|item| item == &value) {
            self.sensitive_values.0.push(value);
            self.sensitive_values
                .0
                .sort_by_key(|value| std::cmp::Reverse(value.len()));
        }
    }

    /// Replaces every exact registered sensitive value in `input`.
    pub fn redact_sensitive(&self, input: &str) -> String {
        self.sensitive_values
            .0
            .iter()
            .fold(input.to_owned(), |redacted, value| {
                redacted.replace(value, "[REDACTED]")
            })
    }

    fn redact_provider_error(&self, error: ProviderError) -> ProviderError {
        match error {
            ProviderError::Remote { message } => ProviderError::Remote {
                message: self.redact_sensitive(&message),
            },
            ProviderError::InvalidResponse { message } => ProviderError::InvalidResponse {
                message: self.redact_sensitive(&message),
            },
            other => other,
        }
    }

    pub async fn run_provider<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        prompt: &str,
        next_seq: u64,
    ) -> Result<u64, ProviderError> {
        self.run_provider_messages(client, &[ProviderMessage::user(prompt)], next_seq)
            .await
    }

    pub async fn run_provider_messages<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        next_seq: u64,
    ) -> Result<u64, ProviderError> {
        self.run_provider_messages_with_tools(client, messages, &[], next_seq)
            .await
    }

    async fn run_provider_messages_with_tools<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        tools: &[Value],
        next_seq: u64,
    ) -> Result<u64, ProviderError> {
        let messages = self.redact_messages(messages);
        let mut app = std::mem::replace(&mut self.app, AppHandle::fake());
        let mut normalizer = ProviderStreamNormalizer::new(
            client.adapter().kind(),
            next_seq,
            self.sensitive_values.0.clone(),
        );
        let stream_result = client
            .stream_messages_with_tools(&messages, tools, |event| {
                normalizer.push(&mut app, event);
            })
            .await;
        let result = match stream_result {
            Ok(()) => normalizer.finish(),
            Err(error) => Err(self.redact_provider_error(error)),
        };
        self.app = app;
        result
    }

    pub async fn run_provider_turn<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        prompt: &str,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
    ) -> Result<(u64, Vec<ToolResult>), ProviderError> {
        self.run_provider_messages_turn(
            client,
            &[ProviderMessage::user(prompt)],
            mode,
            cwd,
            next_seq,
        )
        .await
    }

    pub async fn run_provider_messages_turn<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
    ) -> Result<(u64, Vec<ToolResult>), ProviderError> {
        let event_start = self.app.events().len();
        let tools = self.tools.definitions_for_mode(mode);
        let next_seq = self
            .run_provider_messages_with_tools(client, messages, &tools, next_seq)
            .await?;
        let mut calls = tool_calls_since(&self.app, event_start);
        assign_missing_call_ids(&mut calls, 0);
        let mut next_seq = next_seq;
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let (result, following_seq) =
                self.execute_tool(mode, cwd.as_ref(), &call.name, &call.arguments, next_seq)?;
            next_seq = following_seq;
            results.push(result);
        }
        Ok((next_seq, results))
    }

    pub async fn run_agent_loop<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        prompt: &str,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
        config: AgentLoopConfig,
    ) -> Result<AgentLoopResult, ProviderError> {
        self.run_agent_loop_with_messages(
            client,
            &[ProviderMessage::user(prompt)],
            mode,
            cwd,
            next_seq,
            config,
        )
        .await
    }

    pub async fn run_agent_loop_with_message<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        message: ProviderMessage,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
        config: AgentLoopConfig,
    ) -> Result<AgentLoopResult, ProviderError> {
        self.run_agent_loop_with_messages(client, &[message], mode, cwd, next_seq, config)
            .await
    }

    pub async fn run_agent_loop_with_messages<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        initial_messages: &[ProviderMessage],
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        next_seq: u64,
        config: AgentLoopConfig,
    ) -> Result<AgentLoopResult, ProviderError> {
        if config.max_turns == 0 {
            return Ok(AgentLoopResult {
                next_seq,
                turns: 0,
                stop: AgentLoopStop::TurnLimit,
                tool_results: Vec::new(),
                usage: UsageTotals::default(),
            });
        }
        let cwd = cwd.as_ref();
        let mut messages = self.redact_messages(initial_messages);
        let mut next_seq = next_seq;
        let mut all_results = Vec::new();
        let mut guard = LoopGuard::default();
        let mut turns = 0;
        let mut stop = AgentLoopStop::TurnLimit;
        let mut usage = UsageTotals::default();
        let mut seen_tool_outputs: std::collections::HashSet<u64> =
            std::collections::HashSet::new();
        let mut tools_cache = None;

        for turn in 0..config.max_turns {
            turns = turn + 1;
            let estimated_tokens = estimate_provider_message_tokens(&messages);
            let budget = ContextBudget::new(
                config.context_window_tokens,
                estimated_tokens,
                config.context_reserve_tokens,
            );
            let has_compactable = has_compactable_history(&messages);
            let should_compact =
                config.context_compaction_enabled && budget.should_compact() && has_compactable;
            if !budget.can_fit(config.context_reserve_tokens) && !should_compact {
                return Err(ProviderError::InvalidResponse {
                    message: "context window exceeded and compaction is unavailable".into(),
                });
            }
            if should_compact {
                let (compacted, summary_usage, following_seq) = self
                    .compact_before_send(
                        client,
                        &messages,
                        next_seq,
                        config.context_window_tokens,
                        config.context_reserve_tokens,
                    )
                    .await?;
                messages = compacted;
                usage.input_tokens = usage
                    .input_tokens
                    .saturating_add(summary_usage.input_tokens);
                usage.output_tokens = usage
                    .output_tokens
                    .saturating_add(summary_usage.output_tokens);
                next_seq = following_seq;

                let compacted_tokens = estimate_provider_message_tokens(&messages);
                let compacted_budget = ContextBudget::new(
                    config.context_window_tokens,
                    compacted_tokens,
                    config.context_reserve_tokens,
                );
                if !compacted_budget.can_fit(config.context_reserve_tokens) {
                    return Err(ProviderError::InvalidResponse {
                        message: "context window still exceeded after compaction".into(),
                    });
                }
            }
            let event_start = self.app.events().len();
            let (tools, tools_bytes) = tools_cache.get_or_insert_with(|| {
                let tools = self.tools.definitions_for_mode(mode);
                let tools_bytes = serde_json::to_vec(&tools).map_or(0, |bytes| bytes.len() as u64);
                (tools, tools_bytes)
            });
            let snapshot = crate::SessionEvent::new(
                next_seq,
                crate::EventKind::ContextSnapshot {
                    tools_bytes: *tools_bytes,
                    history_bytes: payload_bytes(&messages),
                },
            );
            self.app
                .push_event(snapshot)
                .map_err(|message| ProviderError::InvalidResponse {
                    message: message.into(),
                })?;
            next_seq += 1;
            next_seq = self
                .run_provider_messages_with_tools(client, &messages, tools.as_slice(), next_seq)
                .await?;
            let mut calls = tool_calls_since(&self.app, event_start);
            for event in self.app.events().get(event_start..).unwrap_or_default() {
                if let crate::EventKind::Usage {
                    input_tokens,
                    output_tokens,
                } = &event.kind
                {
                    usage.add(*input_tokens, *output_tokens);
                }
            }

            if calls.is_empty() {
                stop = AgentLoopStop::ProviderCompleted;
                break;
            }

            let remaining = config.max_tool_calls.saturating_sub(all_results.len());
            if calls.len() > remaining {
                calls.truncate(remaining);
                assign_missing_call_ids(&mut calls, turn);
                let mut results = Vec::with_capacity(calls.len());
                for call in &calls {
                    let (result, following_seq) =
                        self.execute_tool(mode, cwd, &call.name, &call.arguments, next_seq)?;
                    next_seq = following_seq;
                    results.push(result);
                }
                next_seq =
                    self.materialize_results(&mut results, config.max_result_bytes, next_seq)?;
                all_results.extend(results);
                stop = AgentLoopStop::ToolLimit;
                break;
            }

            assign_missing_call_ids(&mut calls, turn);
            let mut results = Vec::with_capacity(calls.len());
            for call in &calls {
                let (result, following_seq) =
                    self.execute_tool(mode, cwd, &call.name, &call.arguments, next_seq)?;
                next_seq = following_seq;
                results.push(result);
            }
            next_seq = self.materialize_results(&mut results, config.max_result_bytes, next_seq)?;
            let tool_calls = calls.clone();
            let assistant_text = self
                .app
                .events()
                .get(event_start..)
                .unwrap_or_default()
                .iter()
                .filter_map(|event| match &event.kind {
                    crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            messages.push(ProviderMessage::assistant(assistant_text, tool_calls));
            for (call, result) in calls.iter().zip(results.iter()) {
                let full_output = prompt_output(result, config.max_result_bytes);
                let output =
                    if result.success && !seen_tool_outputs.insert(tool_output_hash(&full_output))
                    {
                        // Byte-identical rerun of an earlier successful tool:
                        // the content is already in context, so only a
                        // pointer goes on the wire (token dedup, TOK-03).
                        format!(
                            "[duplicate {} result omitted; identical output already in context]",
                            call.name
                        )
                    } else {
                        full_output
                    };
                messages.push(ProviderMessage::tool(
                    call.name.clone(),
                    call.id.clone(),
                    output,
                ));
                if !result.success && !guard.accept(&call.name, &call.arguments, &result.output) {
                    all_results.extend(results.clone());
                    stop = AgentLoopStop::RepeatedFailedTool;
                    self.app
                        .push_event(crate::SessionEvent::new(
                            next_seq,
                            crate::EventKind::TerminalError {
                                message: "repeated failed tool call blocked".into(),
                            },
                        ))
                        .map_err(|message| ProviderError::InvalidResponse {
                            message: message.into(),
                        })?;
                    next_seq += 1;
                    return Ok(AgentLoopResult {
                        next_seq,
                        turns,
                        stop,
                        tool_results: all_results,
                        usage,
                    });
                }
            }
            all_results.extend(results);
            if turn + 1 == config.max_turns {
                stop = AgentLoopStop::TurnLimit;
            }
        }

        Ok(AgentLoopResult {
            next_seq,
            turns,
            stop,
            tool_results: all_results,
            usage,
        })
    }

    async fn compact_before_send<A: ProviderAdapter>(
        &mut self,
        client: &HttpProviderClient<A>,
        messages: &[ProviderMessage],
        next_seq: u64,
        context_window_tokens: u64,
        reserve_tokens: u64,
    ) -> Result<(Vec<ProviderMessage>, UsageTotals, u64), ProviderError> {
        let summary_prompt =
            build_bounded_summary_prompt(messages, context_window_tokens, reserve_tokens).map_err(
                |message| ProviderError::InvalidResponse {
                    message: message.into(),
                },
            )?;
        let summary_request_tokens =
            estimate_provider_message_tokens(&[ProviderMessage::user(summary_prompt.clone())]);
        if !ContextBudget::new(
            context_window_tokens,
            summary_request_tokens,
            reserve_tokens,
        )
        .can_fit(0)
        {
            return Err(ProviderError::InvalidResponse {
                message: "summary request exceeds context window and reserve".into(),
            });
        }
        let events = client
            .send_messages(&[ProviderMessage::user(summary_prompt)])
            .await?;
        let mut summary = String::new();
        let mut usage = UsageTotals::default();
        let mut summary_usage_events = Vec::new();
        for event in events {
            match event {
                ProviderEvent::TextDelta(text) => summary.push_str(&text),
                ProviderEvent::Usage {
                    input_tokens,
                    output_tokens,
                } => {
                    usage.add(input_tokens, output_tokens);
                    summary_usage_events.push((input_tokens, output_tokens));
                }
                _ => {}
            }
        }
        let compacted = compact_provider_messages(messages, summary).map_err(|message| {
            ProviderError::InvalidResponse {
                message: message.into(),
            }
        })?;
        if summary_usage_events.is_empty() {
            summary_usage_events.push((0, 0));
        }
        let mut following_seq = next_seq;
        for (input_tokens, output_tokens) in summary_usage_events {
            self.app
                .push_event(crate::SessionEvent::new(
                    following_seq,
                    crate::EventKind::Usage {
                        input_tokens,
                        output_tokens,
                    },
                ))
                .map_err(|message| ProviderError::InvalidResponse {
                    message: message.into(),
                })?;
            following_seq += 1;
        }
        self.app
            .push_event(crate::SessionEvent::new(
                following_seq,
                crate::EventKind::CompactionCompleted,
            ))
            .map_err(|message| ProviderError::InvalidResponse {
                message: message.into(),
            })?;
        Ok((compacted, usage, following_seq + 1))
    }

    pub fn execute_tool(
        &mut self,
        mode: crate::OperatingMode,
        cwd: impl AsRef<Path>,
        name: &str,
        arguments: &str,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.app
            .push_event(crate::SessionEvent::new(
                next_seq,
                crate::EventKind::ToolStarted { name: name.into() },
            ))
            .map_err(|message| ProviderError::InvalidResponse {
                message: message.into(),
            })?;
        let mut result = self.tools.execute_with_cancellation(
            mode,
            cwd,
            name,
            arguments,
            self.cancellation.as_ref(),
        );
        result.output = self.redact_sensitive(&result.output);
        self.app
            .push_event(crate::SessionEvent::new(
                next_seq + 1,
                crate::EventKind::ToolOutput {
                    name: result.name.clone(),
                    output: result.output.clone(),
                },
            ))
            .map_err(|message| ProviderError::InvalidResponse {
                message: message.into(),
            })?;
        self.app
            .push_event(crate::SessionEvent::new(
                next_seq + 2,
                crate::EventKind::ToolFinished {
                    name: result.name.clone(),
                    success: result.success,
                },
            ))
            .map_err(|message| ProviderError::InvalidResponse {
                message: message.into(),
            })?;
        Ok((result, next_seq + 3))
    }

    fn redact_messages(&self, messages: &[ProviderMessage]) -> Vec<ProviderMessage> {
        messages
            .iter()
            .cloned()
            .map(|mut message| {
                message.content = self.redact_sensitive(&message.content);
                message.name = message.name.map(|value| self.redact_sensitive(&value));
                message.tool_call_id = message
                    .tool_call_id
                    .map(|value| self.redact_sensitive(&value));
                for call in &mut message.tool_calls {
                    call.id = self.redact_sensitive(&call.id);
                    call.name = self.redact_sensitive(&call.name);
                    call.arguments = self.redact_sensitive(&call.arguments);
                }
                for block in &mut message.content_blocks {
                    match block {
                        crate::provider::ProviderContentBlock::Text(text)
                        | crate::provider::ProviderContentBlock::Unsupported { kind: text } => {
                            *text = self.redact_sensitive(text);
                        }
                        crate::provider::ProviderContentBlock::Image { data, .. }
                        | crate::provider::ProviderContentBlock::Audio { data, .. }
                        | crate::provider::ProviderContentBlock::File { data, .. } => {
                            *data = self.redact_sensitive(data);
                        }
                    }
                }
                message
            })
            .collect()
    }

    fn materialize_results(
        &mut self,
        results: &mut [ToolResult],
        max_result_bytes: usize,
        mut next_seq: u64,
    ) -> Result<u64, ProviderError> {
        let Some(store) = self.artifact_store.as_ref() else {
            return Ok(next_seq);
        };
        for result in results {
            if result.output.len() <= max_result_bytes {
                continue;
            }
            let handle = store
                .put(&format!("tool-{}", result.name), result.output.as_bytes())
                .map_err(|error| ProviderError::InvalidResponse {
                    message: format!("artifact: {error}"),
                })?;
            result.artifact = Some(handle.clone());
            self.app
                .push_event(crate::SessionEvent::new(
                    next_seq,
                    crate::EventKind::ArtifactStored {
                        id: handle.id,
                        size: handle.size,
                    },
                ))
                .map_err(|message| ProviderError::InvalidResponse {
                    message: message.into(),
                })?;
            next_seq += 1;
        }
        Ok(next_seq)
    }
}

fn tool_calls_since(app: &AppHandle, event_start: usize) -> Vec<ProviderToolCall> {
    app.events()
        .get(event_start..)
        .unwrap_or_default()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ProviderToolCall {
                id,
                name,
                arguments,
            } => Some(ProviderToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            crate::EventKind::ToolCall { name, arguments } => Some(ProviderToolCall {
                id: String::new(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

#[derive(Clone, Debug)]
struct BufferedToolCall {
    index: Option<u32>,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    legacy_seen: bool,
    input_delta_seen: bool,
}

impl BufferedToolCall {
    fn new(index: Option<u32>, id: Option<String>, name: Option<String>) -> Self {
        Self {
            index,
            id,
            name,
            arguments: String::new(),
            legacy_seen: false,
            input_delta_seen: false,
        }
    }
}

struct ProviderStreamNormalizer {
    kind: ProviderKind,
    next_seq: u64,
    openai_calls: Vec<BufferedToolCall>,
    anthropic_calls: Vec<BufferedToolCall>,
    standalone_calls: Vec<BufferedToolCall>,
    sensitive_values: Vec<String>,
    text_pending: String,
    reasoning_pending: String,
    stopped: bool,
    error: Option<ProviderError>,
}

impl ProviderStreamNormalizer {
    fn new(kind: ProviderKind, next_seq: u64, sensitive_values: Vec<String>) -> Self {
        Self {
            kind,
            next_seq,
            openai_calls: Vec::new(),
            anthropic_calls: Vec::new(),
            standalone_calls: Vec::new(),
            sensitive_values,
            text_pending: String::new(),
            reasoning_pending: String::new(),
            stopped: false,
            error: None,
        }
    }

    fn push(&mut self, app: &mut AppHandle, event: ProviderEvent) {
        if self.error.is_some() {
            return;
        }
        if let Err(error) = self.push_inner(app, event) {
            self.error = Some(error);
        }
    }

    fn push_inner(
        &mut self,
        app: &mut AppHandle,
        event: ProviderEvent,
    ) -> Result<(), ProviderError> {
        if self.stopped {
            return Err(ProviderError::InvalidResponse {
                message: "provider emitted events after stop".into(),
            });
        }
        match event {
            ProviderEvent::TextDelta(text) => {
                let text = take_redacted_stream_chunk(
                    &mut self.text_pending,
                    &text,
                    &self.sensitive_values,
                    false,
                );
                if text.is_empty() {
                    Ok(())
                } else {
                    push_runtime_event(
                        app,
                        &mut self.next_seq,
                        crate::EventKind::AssistantTextDelta { text },
                    )
                }
            }
            ProviderEvent::ReasoningDelta(text) => {
                let text = take_redacted_stream_chunk(
                    &mut self.reasoning_pending,
                    &text,
                    &self.sensitive_values,
                    false,
                );
                if text.is_empty() {
                    Ok(())
                } else {
                    push_runtime_event(
                        app,
                        &mut self.next_seq,
                        crate::EventKind::ReasoningDelta { text },
                    )
                }
            }
            ProviderEvent::Usage {
                input_tokens,
                output_tokens,
            } => push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::Usage {
                    input_tokens,
                    output_tokens,
                },
            ),
            ProviderEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } if matches!(
                self.kind,
                ProviderKind::OpenAiCompatible | ProviderKind::OpenAiCodex
            ) =>
            {
                append_openai_delta(&mut self.openai_calls, index, id, name, arguments)
            }
            ProviderEvent::ToolCallStart { index, id, name }
                if self.kind == ProviderKind::Anthropic =>
            {
                start_anthropic_call(&mut self.anthropic_calls, index, id, name)
            }
            ProviderEvent::ToolCallInputDelta {
                index,
                partial_json,
            } if self.kind == ProviderKind::Anthropic => {
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
                call.arguments.push_str(&partial_json);
                Ok(())
            }
            ProviderEvent::ContentBlockStop { index } if self.kind == ProviderKind::Anthropic => {
                let Some(position) = self
                    .anthropic_calls
                    .iter()
                    .position(|call| call.index == Some(index))
                else {
                    return Ok(());
                };
                let call = redact_buffered_call(
                    self.anthropic_calls.remove(position),
                    &self.sensitive_values,
                );
                publish_buffered_call(app, &mut self.next_seq, call)
            }
            ProviderEvent::ToolCall { name, arguments } => {
                if self.kind == ProviderKind::Anthropic
                    && attach_legacy_call(&mut self.anthropic_calls, &name, &arguments)?
                {
                    return Ok(());
                }
                if matches!(
                    self.kind,
                    ProviderKind::OpenAiCompatible | ProviderKind::OpenAiCodex
                ) && attach_legacy_call(&mut self.openai_calls, &name, &arguments)?
                {
                    return Ok(());
                }
                validate_tool_arguments(&name, &arguments)?;
                self.standalone_calls.push(BufferedToolCall {
                    index: None,
                    id: None,
                    name: Some(name),
                    arguments,
                    legacy_seen: true,
                    input_delta_seen: false,
                });
                Ok(())
            }
            ProviderEvent::Stopped { reason } => {
                if !self.anthropic_calls.is_empty() {
                    return Err(ProviderError::MalformedToolCall);
                }
                self.flush_text(app)?;
                if matches!(
                    self.kind,
                    ProviderKind::OpenAiCompatible | ProviderKind::OpenAiCodex
                ) {
                    let mut calls = std::mem::take(&mut self.openai_calls);
                    calls.sort_by_key(|call| call.index.unwrap_or(u32::MAX));
                    for call in calls {
                        let call = redact_buffered_call(call, &self.sensitive_values);
                        publish_buffered_call(app, &mut self.next_seq, call)?;
                    }
                }
                for call in std::mem::take(&mut self.standalone_calls) {
                    let call = redact_buffered_call(call, &self.sensitive_values);
                    publish_buffered_call(app, &mut self.next_seq, call)?;
                }
                push_runtime_event(
                    app,
                    &mut self.next_seq,
                    crate::EventKind::AssistantEnded {
                        reason: redact_values(&self.sensitive_values, &reason),
                    },
                )?;
                self.stopped = true;
                Ok(())
            }
            ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::ToolCallStart { .. }
            | ProviderEvent::ToolCallInputDelta { .. }
            | ProviderEvent::ContentBlockStop { .. } => Err(ProviderError::MalformedToolCall),
        }
    }

    fn flush_text(&mut self, app: &mut AppHandle) -> Result<(), ProviderError> {
        let text =
            take_redacted_stream_chunk(&mut self.text_pending, "", &self.sensitive_values, true);
        if !text.is_empty() {
            push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::AssistantTextDelta { text },
            )?;
        }
        let reasoning = take_redacted_stream_chunk(
            &mut self.reasoning_pending,
            "",
            &self.sensitive_values,
            true,
        );
        if !reasoning.is_empty() {
            push_runtime_event(
                app,
                &mut self.next_seq,
                crate::EventKind::ReasoningDelta { text: reasoning },
            )?;
        }
        Ok(())
    }

    fn finish(self) -> Result<u64, ProviderError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !self.stopped || !self.openai_calls.is_empty() || !self.anthropic_calls.is_empty() {
            return Err(ProviderError::MalformedToolCall);
        }
        Ok(self.next_seq)
    }
}

fn take_redacted_stream_chunk(
    pending: &mut String,
    delta: &str,
    sensitive_values: &[String],
    flush: bool,
) -> String {
    pending.push_str(delta);
    let split_at = if flush {
        pending.len()
    } else {
        safe_stream_split(pending, sensitive_values)
    };
    let tail = pending[split_at..].to_owned();
    let ready = redact_values(sensitive_values, &pending[..split_at]);
    *pending = tail;
    ready
}

fn safe_stream_split(input: &str, sensitive_values: &[String]) -> usize {
    let held_bytes = sensitive_values
        .iter()
        .flat_map(|value| {
            value
                .char_indices()
                .skip(1)
                .map(move |(index, _)| &value[..index])
        })
        .filter(|prefix| input.ends_with(prefix))
        .map(str::len)
        .max()
        .unwrap_or(0);
    let mut split_at = input.len().saturating_sub(held_bytes);
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

fn redact_values(sensitive_values: &[String], input: &str) -> String {
    sensitive_values
        .iter()
        .fold(input.to_owned(), |redacted, value| {
            redacted.replace(value, "[REDACTED]")
        })
}

fn redact_buffered_call(
    mut call: BufferedToolCall,
    sensitive_values: &[String],
) -> BufferedToolCall {
    call.id = call.id.map(|value| redact_values(sensitive_values, &value));
    call.name = call
        .name
        .map(|value| redact_values(sensitive_values, &value));
    call.arguments = redact_values(sensitive_values, &call.arguments);
    call
}

fn push_runtime_event(
    app: &mut AppHandle,
    next_seq: &mut u64,
    kind: crate::EventKind,
) -> Result<(), ProviderError> {
    app.push_event(crate::SessionEvent::new(*next_seq, kind))
        .map_err(|message| ProviderError::InvalidResponse {
            message: message.into(),
        })?;
    *next_seq += 1;
    Ok(())
}

fn append_openai_delta(
    calls: &mut Vec<BufferedToolCall>,
    index: Option<u32>,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
) -> Result<(), ProviderError> {
    let id = id.filter(|id| !id.is_empty());
    let by_index = index.and_then(|value| calls.iter().position(|call| call.index == Some(value)));
    let by_id = id.as_ref().and_then(|value| {
        calls
            .iter()
            .position(|call| call.id.as_ref() == Some(value))
    });
    if let (Some(index_position), Some(id_position)) = (by_index, by_id) {
        if index_position != id_position {
            return Err(ProviderError::MalformedToolCall);
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
            _ => return Err(ProviderError::MalformedToolCall),
        }
    } else {
        if calls.iter().any(|call| {
            index.is_some_and(|value| call.index == Some(value))
                || id
                    .as_ref()
                    .is_some_and(|value| call.id.as_ref() == Some(value))
        }) {
            return Err(ProviderError::MalformedToolCall);
        }
        calls.push(BufferedToolCall::new(index, id.clone(), name.clone()));
        calls.len() - 1
    };
    let call = &mut calls[position];
    if let (Some(existing), Some(incoming)) = (&call.index, index) {
        if *existing != incoming {
            return Err(ProviderError::MalformedToolCall);
        }
    } else if call.index.is_none() {
        call.index = index;
    }
    if let (Some(existing), Some(incoming)) = (&call.id, &id) {
        if existing != incoming {
            return Err(ProviderError::MalformedToolCall);
        }
    } else if call.id.is_none() {
        call.id = id;
    }
    if let Some(incoming) = name {
        if let Some(existing) = &call.name {
            if existing != &incoming {
                return Err(ProviderError::MalformedToolCall);
            }
        } else {
            call.name = Some(incoming);
        }
    }
    call.arguments.push_str(&arguments);
    Ok(())
}

fn start_anthropic_call(
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

fn attach_legacy_call(
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
        call.arguments = arguments.to_owned();
        call.legacy_seen = true;
    } else if call.arguments != arguments {
        if serde_json::from_str::<Value>(&call.arguments).is_err()
            && serde_json::from_str::<Value>(arguments).is_ok()
        {
            call.arguments = arguments.to_owned();
            call.legacy_seen = true;
        } else {
            return Err(ProviderError::MalformedToolCall);
        }
    }
    Ok(true)
}

fn publish_buffered_call(
    app: &mut AppHandle,
    next_seq: &mut u64,
    call: BufferedToolCall,
) -> Result<(), ProviderError> {
    let Some(name) = call.name.filter(|name| !name.is_empty()) else {
        return Err(ProviderError::MalformedToolCall);
    };
    validate_tool_arguments(&name, &call.arguments)?;
    push_runtime_event(
        app,
        next_seq,
        crate::EventKind::ProviderToolCall {
            id: call.id.unwrap_or_default(),
            name,
            arguments: call.arguments,
        },
    )
}

fn validate_tool_arguments(name: &str, arguments: &str) -> Result<(), ProviderError> {
    if name.is_empty() || serde_json::from_str::<Value>(arguments).is_err() {
        Err(ProviderError::MalformedToolCall)
    } else {
        Ok(())
    }
}

fn assign_missing_call_ids(calls: &mut [ProviderToolCall], turn: usize) {
    for (index, call) in calls.iter_mut().enumerate() {
        if call.id.is_empty() {
            call.id = format!("slim-call-{turn}-{index}");
        }
    }
}

/// Wire-size of the message history in bytes (role, content, tool calls and
/// multimodal payloads). Used by the per-turn `ContextSnapshot` telemetry.
fn payload_bytes(messages: &[ProviderMessage]) -> u64 {
    messages
        .iter()
        .map(|message| {
            let mut bytes = message.role.len() + message.content.len();
            bytes += message.name.as_deref().map_or(0, str::len);
            bytes += message.tool_call_id.as_deref().map_or(0, str::len);
            bytes += message
                .tool_calls
                .iter()
                .map(|call| call.id.len() + call.name.len() + call.arguments.len())
                .sum::<usize>();
            bytes += message
                .content_blocks
                .iter()
                .map(|block| match block {
                    crate::provider::ProviderContentBlock::Text(text) => text.len(),
                    crate::provider::ProviderContentBlock::Image { media_type, data }
                    | crate::provider::ProviderContentBlock::Audio { media_type, data }
                    | crate::provider::ProviderContentBlock::File { media_type, data } => {
                        media_type.len() + data.len()
                    }
                    crate::provider::ProviderContentBlock::Unsupported { kind } => kind.len(),
                })
                .sum::<usize>();
            bytes as u64
        })
        .sum()
}

fn tool_output_hash(output: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    output.hash(&mut hasher);
    hasher.finish()
}

fn truncate_result(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_owned();
    }
    let mut end = max_bytes;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &output[..end])
}

fn prompt_output(result: &ToolResult, max_bytes: usize) -> String {
    if result.output.len() <= max_bytes {
        return result.output.clone();
    }
    let preview = truncate_result(&result.output, max_bytes);
    if let Some(ArtifactHandle { id, size, path }) = &result.artifact {
        format!(
            "{preview}\n[artifact id={id} size={size} path={}]",
            path.display()
        )
    } else {
        preview
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}
