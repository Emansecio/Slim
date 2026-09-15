use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use slim_core::OperatingMode;

use crate::api::{
    BlockId, ClinePassCatalogSource, CommandCodeCatalogSource, ContentRequestId,
    InteractionRequestId, LoginProvider, ModelAlias, OpenCodeCatalogSource, OpenCodeModelView,
    ReasoningEffort, SensitiveText, SessionId, TranscriptMessage, TranscriptRole, UiCommand,
    UiEvent, ZenCatalogSource,
};
use crate::block::{
    Block, BlockKind, BlockLifecycle, FoldState, InteractionRequestKind, InteractionRequestState,
    PendingContentPage, ToolState,
};
use crate::composer::Composer;
use crate::inspector::{InspectorState, SearchState};

/// Populates a vec of `OpenCodeModelView` from the static ClinePass model catalog.
fn default_clinepass_models() -> Vec<OpenCodeModelView> {
    slim_core::provider::clinepass_models()
        .iter()
        .map(|m| OpenCodeModelView {
            id: m.id.to_owned(),
            name: m.name.to_owned(),
            context_window_tokens: m.context_window,
            max_output_tokens: m.max_output_tokens as u64,
            reasoning_levels: Vec::new(),
            accepts_images: m.accepts_images,
        })
        .collect()
}

fn default_command_code_models() -> Vec<OpenCodeModelView> {
    slim_core::provider::command_code_models()
        .iter()
        .map(|m| OpenCodeModelView {
            id: m.id.to_owned(),
            name: m.name.to_owned(),
            context_window_tokens: m.context_window,
            max_output_tokens: 0,
            reasoning_levels: Vec::new(),
            accepts_images: false,
        })
        .collect()
}

fn default_zen_models() -> Vec<OpenCodeModelView> {
    slim_core::provider::zen_models()
        .iter()
        .map(|m| OpenCodeModelView {
            id: m.id.to_owned(),
            name: m.name.to_owned(),
            context_window_tokens: m.context_window.unwrap_or_default(),
            max_output_tokens: m.max_output_tokens.unwrap_or_default() as u64,
            reasoning_levels: m
                .reasoning_levels
                .iter()
                .filter_map(|level| ReasoningEffort::parse(level))
                .collect(),
            accepts_images: m.accepts_images,
        })
        .collect()
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RevisionSet {
    pub content: u64,
    pub fold: u64,
    pub theme: u64,
    pub viewport: u64,
    pub focus: u64,
    pub status: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScrollAnchor {
    pub block_id: BlockId,
    pub row_offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FollowMode {
    LiveEdge { prompt_id: Option<BlockId> },
    Pinned(ScrollAnchor),
    Top,
}

impl Default for FollowMode {
    fn default() -> Self {
        Self::LiveEdge { prompt_id: None }
    }
}

/// Stable scroll state (§13.3): pinned views address a block/physical row,
/// while live edge may page-fill from the most recently submitted prompt.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScrollState {
    pub mode: FollowMode,
    pub unseen: u32,
}

impl ScrollState {
    pub fn is_pinned(&self) -> bool {
        matches!(self.mode, FollowMode::Pinned(_) | FollowMode::Top)
    }

    pub fn is_live_edge(&self) -> bool {
        matches!(self.mode, FollowMode::LiveEdge { .. })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameClock {
    pub frame: u64,
    pub elapsed_ms: u64,
}

/// Info toasts expire on the injected clock (§15.8). Errors that become blocks
/// are not stored here.
pub const INFO_TOAST_TTL_MS: u64 = 5_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    pub message: String,
    pub created_ms: u64,
}

impl Notification {
    pub fn as_str(&self) -> &str {
        &self.message
    }

    fn is_visible(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.created_ms) < INFO_TOAST_TTL_MS
    }
}

impl std::ops::Deref for Notification {
    type Target = str;

    fn deref(&self) -> &str {
        &self.message
    }
}

impl AsRef<str> for Notification {
    fn as_ref(&self) -> &str {
        &self.message
    }
}

impl From<&str> for Notification {
    fn from(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            created_ms: 0,
        }
    }
}

impl From<String> for Notification {
    fn from(message: String) -> Self {
        Self {
            message,
            created_ms: 0,
        }
    }
}

impl PartialEq<str> for Notification {
    fn eq(&self, other: &str) -> bool {
        self.message == other
    }
}

impl PartialEq<&str> for Notification {
    fn eq(&self, other: &&str) -> bool {
        self.message == *other
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityPhase {
    Thinking,
    Responding,
    AwaitingProvider,
    RunningTool(String),
    WaitingForInput,
    External(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityState {
    pub phase: ActivityPhase,
    pub started_ms: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderTimingState {
    pub headers_ms: Option<u64>,
    pub first_byte_ms: Option<u64>,
    pub first_semantic_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalTail {
    run_id: u64,
    lifecycle: BlockLifecycle,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum LoginStage {
    #[default]
    Providers,
    ApiKey(SensitiveText),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoginOverlay {
    pub selected: usize,
    pub stage: LoginStage,
    pub in_progress: bool,
    pub progress: Option<String>,
    pub auth_url: Option<SensitiveText>,
    pub user_code: Option<SensitiveText>,
}

impl LoginOverlay {
    pub fn provider(&self) -> LoginProvider {
        match self.selected {
            0 => LoginProvider::Anthropic,
            1 => LoginProvider::OpenAiCodex,
            2 => LoginProvider::OpenCodeGo,
            3 => LoginProvider::ClinePass,
            4 => LoginProvider::CommandCode,
            5 => LoginProvider::Xai,
            _ => LoginProvider::OpenCodeZen,
        }
    }
}

/// Grouped model overlay (G233/G234): each provider is a collapsible group;
/// Space toggles collapse; the filter narrows across all groups.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelOverlay {
    pub selected: usize,
    pub viewport_start: usize,
    /// Case-insensitive substring filter typed by the user (§15.7-style).
    pub filter: String,
    /// Collapsed state per group: 0 = OpenAI Codex, 1 = OpenCode Go,
    /// 2 = ClinePass, 3 = Command Code, 4 = OpenCode Zen.
    pub collapsed: [bool; 5],
}

/// `/mcp` overlay: flat server list with an inline remove confirmation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpOverlay {
    pub selected: usize,
    pub viewport_start: usize,
    /// Server name armed for removal; `y`/`Enter` confirms, `n`/`Esc` cancels.
    pub confirm_remove: Option<String>,
}

/// A single flattened row in the grouped overlay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelRow {
    /// Collapsible group header, carrying the group index.
    Header(usize),
    /// An OpenAI Codex alias (Astra/Sol/Terra/Luna).
    Alias(ModelAlias),
    /// An OpenCode Go catalog model, carrying its index into
    /// `AppState::open_code_models`.
    Catalog(usize),
    /// A ClinePass model, carrying its index into `AppState::cline_pass_models`.
    ClinePass(usize),
    /// A Command Code model, carrying its index into
    /// `AppState::command_code_models`.
    CommandCode(usize),
    /// An OpenCode Zen free-tier model, carrying its index into
    /// `AppState::zen_models`.
    Zen(usize),
}

impl ModelOverlay {
    /// Builds the flattened row list respecting the filter and collapsed state.
    pub fn rows(
        &self,
        opencode_models: &[OpenCodeModelView],
        clinepass_models: &[OpenCodeModelView],
        command_code_models: &[OpenCodeModelView],
        zen_models: &[OpenCodeModelView],
    ) -> Vec<ModelRow> {
        let query = self.filter.to_lowercase();
        let mut out = Vec::new();

        // Group 0: OpenAI Codex built-in aliases
        let codex_hidden = self.collapsed[0] && query.is_empty();
        out.push(ModelRow::Header(0));
        if !codex_hidden {
            for alias in &ModelAlias::ALL {
                if query.is_empty()
                    || alias.id().contains(&query)
                    || alias.label().to_lowercase().contains(&query)
                {
                    out.push(ModelRow::Alias(*alias));
                }
            }
        }

        // Group 1: OpenCode Go catalog
        let opencode_hidden = self.collapsed[1] && query.is_empty();
        out.push(ModelRow::Header(1));
        if !opencode_hidden {
            for (index, model) in opencode_models.iter().enumerate() {
                if query.is_empty()
                    || model.id.to_lowercase().contains(&query)
                    || model.name.to_lowercase().contains(&query)
                {
                    out.push(ModelRow::Catalog(index));
                }
            }
        }

        // Group 2: ClinePass catalog
        let clinepass_hidden = self.collapsed[2] && query.is_empty();
        out.push(ModelRow::Header(2));
        if !clinepass_hidden {
            for (index, model) in clinepass_models.iter().enumerate() {
                if query.is_empty()
                    || model.id.to_lowercase().contains(&query)
                    || model.name.to_lowercase().contains(&query)
                {
                    out.push(ModelRow::ClinePass(index));
                }
            }
        }

        // Group 3: Command Code catalog
        let command_code_hidden = self.collapsed[3] && query.is_empty();
        out.push(ModelRow::Header(3));
        if !command_code_hidden {
            for (index, model) in command_code_models.iter().enumerate() {
                if query.is_empty()
                    || model.id.to_lowercase().contains(&query)
                    || model.name.to_lowercase().contains(&query)
                {
                    out.push(ModelRow::CommandCode(index));
                }
            }
        }

        // Group 4: OpenCode Zen free-tier catalog
        let zen_hidden = self.collapsed[4] && query.is_empty();
        out.push(ModelRow::Header(4));
        if !zen_hidden {
            for (index, model) in zen_models.iter().enumerate() {
                if query.is_empty()
                    || model.id.to_lowercase().contains(&query)
                    || model.name.to_lowercase().contains(&query)
                {
                    out.push(ModelRow::Zen(index));
                }
            }
        }

        out
    }

    /// Toggles the collapsed state of a provider group.
    pub fn toggle_collapsed(&mut self, group: usize) {
        if group < 5 {
            self.collapsed[group] = !self.collapsed[group];
        }
    }

    /// Returns a fresh overlay with the selection positioned at the currently
    /// active model, or the first non-header row as fallback.
    pub fn for_current(
        current_model: &str,
        active_provider: Option<LoginProvider>,
        opencode_models: &[OpenCodeModelView],
        clinepass_models: &[OpenCodeModelView],
        command_code_models: &[OpenCodeModelView],
        zen_models: &[OpenCodeModelView],
    ) -> Self {
        let active_group = if ModelAlias::parse(current_model).is_some() {
            0
        } else if opencode_models
            .iter()
            .any(|model| model.id == current_model)
        {
            1
        } else if clinepass_models
            .iter()
            .any(|model| model.id == current_model)
        {
            2
        } else if command_code_models
            .iter()
            .any(|model| model.id == current_model)
        {
            3
        } else if zen_models.iter().any(|model| model.id == current_model) {
            4
        } else {
            active_provider.map_or(0, |provider| match provider {
                LoginProvider::OpenAiCodex | LoginProvider::Anthropic => 0,
                LoginProvider::OpenCodeGo => 1,
                LoginProvider::ClinePass => 2,
                LoginProvider::CommandCode => 3,
                LoginProvider::OpenCodeZen => 4,
                // xAI tem grupo próprio adiado no /models; cai no grupo 0.
                LoginProvider::Xai => 0,
            })
        };
        let mut overlay = Self {
            collapsed: [true; 5],
            ..Self::default()
        };
        overlay.collapsed[active_group] = false;
        let rows = overlay.rows(
            opencode_models,
            clinepass_models,
            command_code_models,
            zen_models,
        );
        overlay.selected = rows
            .iter()
            .position(|row| match row {
                ModelRow::Alias(alias) => alias.id() == current_model,
                ModelRow::Catalog(idx) => opencode_models
                    .get(*idx)
                    .is_some_and(|m| m.id == current_model),
                ModelRow::ClinePass(idx) => clinepass_models
                    .get(*idx)
                    .is_some_and(|m| m.id == current_model),
                ModelRow::CommandCode(idx) => command_code_models
                    .get(*idx)
                    .is_some_and(|m| m.id == current_model),
                ModelRow::Zen(idx) => zen_models.get(*idx).is_some_and(|m| m.id == current_model),
                _ => false,
            })
            .unwrap_or_else(|| {
                rows.iter()
                    .position(|r| !matches!(r, ModelRow::Header(_)))
                    .unwrap_or(0)
            });
        overlay
    }
}

/// Slash autocomplete state (W7): `query` is the text after `/` in the token
/// under edit; `selected` indexes into the filtered command list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlashSuggestions {
    pub query: String,
    pub selected: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffortOverlay {
    pub model: ModelAlias,
    pub selected: usize,
    pub fast: bool,
}

impl EffortOverlay {
    pub fn effort(&self) -> ReasoningEffort {
        ReasoningEffort::supported(self.model)
            .get(self.selected)
            .copied()
            .unwrap_or_else(|| ReasoningEffort::default_for(self.model))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppState {
    pub session_id: Option<SessionId>,
    pub cwd: String,
    pub mode: OperatingMode,
    pub model: String,
    pub effort: ReasoningEffort,
    pub codex_fast: bool,
    pub auth_provider: Option<LoginProvider>,
    pub authenticated: bool,
    pub login_overlay: Option<LoginOverlay>,
    pub model_overlay: Option<ModelOverlay>,
    pub open_code_models: Vec<OpenCodeModelView>,
    pub cline_pass_models: Vec<OpenCodeModelView>,
    pub command_code_models: Vec<OpenCodeModelView>,
    pub zen_models: Vec<OpenCodeModelView>,
    pub open_code_catalog_source: Option<OpenCodeCatalogSource>,
    pub cline_pass_catalog_source: Option<ClinePassCatalogSource>,
    pub command_code_catalog_source: Option<CommandCodeCatalogSource>,
    pub zen_catalog_source: Option<ZenCatalogSource>,
    /// Bumped whenever a model catalog Vec is replaced; render memos key on it
    /// because catalog swaps can otherwise reuse a stale row list.
    pub catalog_revision: u64,
    skill_names: Vec<String>,
    /// Bumped whenever `skill_names` is replaced so render memos can key on
    /// the skill list without re-scanning it.
    skills_revision: u64,
    pub mcp_overlay: Option<McpOverlay>,
    pub mcp_servers: Vec<crate::api::McpServerView>,
    /// Bumped on every `McpServersChanged` so render memos key on it.
    pub mcp_revision: u64,
    pub effort_overlay: Option<EffortOverlay>,
    /// Command palette query while open (None = closed).
    pub palette_query: Option<String>,
    pub palette_selected: usize,
    pub palette_viewport_start: usize,
    /// Slash autocomplete while the token under edit matches a command
    /// (None = closed). Triggered by `/` anywhere in the draft.
    pub slash_suggestions: Option<SlashSuggestions>,
    pub inspector: InspectorState,
    pub search: Option<SearchState>,
    blocks: Vec<Block>,
    block_ids: HashSet<Arc<str>>,
    pub notifications: Vec<Notification>,
    pub composer: Composer,
    pub attachment_labels: Vec<String>,
    pub todo_items: Vec<crate::api::TodoItemView>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub input_tokens_overflowed: bool,
    pub output_tokens_overflowed: bool,
    pub context_tokens: u64,
    pub context_window_tokens: u64,
    pub context_exact: bool,
    pub compaction_status: slim_core::context::CompactionStatus,
    context_run_id: Option<u64>,
    context_request_id: Option<u64>,
    pub stream_output_chars: u64,
    request_context_base_tokens: u64,
    request_usage: Option<(u64, u64)>,
    request_usage_finalized: bool,
    request_usage_overflowed: bool,
    request_estimate_open: bool,
    pub working: bool,
    active_run_id: Option<u64>,
    latest_run_assistant: Option<BlockId>,
    terminal_tail: Option<TerminalTail>,
    thinking_open: bool,
    snapshot_resync_needed: bool,
    pub(crate) run_started_ms: Option<u64>,
    pub todo_dock_open: bool,
    /// FIFO of prompts submitted (or steered) while a run is active (§7.4);
    /// drained one-per-turn at run boundaries.
    pub(crate) queued_prompts: VecDeque<String>,
    pub scroll: ScrollState,
    pub clock: FrameClock,
    pub activity: Option<ActivityState>,
    pub provider_timings: ProviderTimingState,
    pub max_mutating_tool_calls: usize,
    pub max_read_tool_calls: usize,
    pub max_turns: usize,
    pub tools_used_read: usize,
    pub tools_used_mutating: usize,
    pub turns_used: usize,
    tool_budget_warned: bool,
    turn_budget_warned: bool,
    pub next_block_id: u64,
    next_content_request_id: u64,
    pub shutdown: bool,
    pub revisions: RevisionSet,
    /// Completed request throughput in tenths of tok/s, timed by the runtime.
    pub last_tok_per_sec: Option<u64>,
    pub last_tok_per_sec_estimated: bool,
    pub selection: Option<crate::selection::ScreenSelection>,
    pub selection_area: Option<ratatui::layout::Rect>,
    pub selection_text: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            session_id: None,
            cwd: String::new(),
            mode: OperatingMode::Auto,
            model: ModelAlias::Sol.id().into(),
            effort: ReasoningEffort::High,
            codex_fast: false,
            auth_provider: None,
            authenticated: false,
            login_overlay: None,
            model_overlay: None,
            open_code_models: Vec::new(),
            cline_pass_models: default_clinepass_models(),
            command_code_models: default_command_code_models(),
            zen_models: default_zen_models(),
            open_code_catalog_source: None,
            cline_pass_catalog_source: None,
            command_code_catalog_source: None,
            zen_catalog_source: None,
            catalog_revision: 0,
            skill_names: Vec::new(),
            skills_revision: 0,
            mcp_overlay: None,
            mcp_servers: Vec::new(),
            mcp_revision: 0,
            effort_overlay: None,
            palette_query: None,
            palette_selected: 0,
            palette_viewport_start: 0,
            slash_suggestions: None,
            inspector: InspectorState::default(),
            search: None,
            blocks: Vec::new(),
            block_ids: HashSet::new(),
            notifications: Vec::new(),
            composer: Composer::default(),
            attachment_labels: Vec::new(),
            todo_items: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            input_tokens_overflowed: false,
            output_tokens_overflowed: false,
            context_tokens: 0,
            context_window_tokens: 0,
            context_exact: false,
            compaction_status: slim_core::context::CompactionStatus::Idle,
            context_run_id: None,
            context_request_id: None,
            stream_output_chars: 0,
            request_context_base_tokens: 0,
            request_usage: None,
            request_usage_finalized: false,
            request_usage_overflowed: false,
            request_estimate_open: false,
            working: false,
            queued_prompts: VecDeque::new(),
            active_run_id: None,
            latest_run_assistant: None,
            terminal_tail: None,
            thinking_open: false,
            snapshot_resync_needed: false,
            run_started_ms: None,
            todo_dock_open: false,
            scroll: ScrollState::default(),
            clock: FrameClock::default(),
            activity: None,
            provider_timings: ProviderTimingState::default(),
            max_mutating_tool_calls: UiEvent::DEFAULT_MAX_MUTATING_TOOL_CALLS,
            max_read_tool_calls: UiEvent::DEFAULT_MAX_READ_TOOL_CALLS,
            max_turns: UiEvent::DEFAULT_MAX_TURNS,
            tools_used_read: 0,
            tools_used_mutating: 0,
            turns_used: 0,
            tool_budget_warned: false,
            turn_budget_warned: false,
            next_block_id: 0,
            next_content_request_id: 1,
            shutdown: false,
            revisions: RevisionSet::default(),
            last_tok_per_sec: None,
            last_tok_per_sec_estimated: false,
            selection: None,
            selection_area: None,
            selection_text: String::new(),
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn skill_names(&self) -> &[String] {
        &self.skill_names
    }

    /// Monotonic counter bumped on every `skill_names` replacement; slash
    /// suggestion memos key on it instead of scanning the list per frame.
    pub(crate) fn skills_revision(&self) -> u64 {
        self.skills_revision
    }

    #[cfg(test)]
    pub(crate) fn set_skill_names_for_test(&mut self, names: Vec<String>) {
        self.skill_names = names;
        self.skills_revision = self.skills_revision.wrapping_add(1);
    }

    /// Workspace plus its already-discovered skill names (discovery runs off
    /// the UI thread, at the event producer).
    fn set_workspace(&mut self, cwd: String, skill_names: Vec<String>) {
        self.cwd = cwd;
        if self.skill_names != skill_names {
            self.skill_names = skill_names;
            self.skills_revision = self.skills_revision.wrapping_add(1);
        }
    }

    pub fn apply_snapshot(&mut self, session_id: SessionId, cwd: String, skill_names: Vec<String>) {
        self.session_id = Some(session_id);
        self.set_workspace(cwd, skill_names);
        self.thinking_open = false;
        self.snapshot_resync_needed = false;
        self.scroll = ScrollState::default();
        self.last_tok_per_sec = None;
        self.last_tok_per_sec_estimated = false;
        self.revisions.status += 1;
        self.revisions.viewport += 1;
    }

    fn restore_session(
        &mut self,
        session_id: SessionId,
        cwd: String,
        messages: Vec<TranscriptMessage>,
        skill_names: Vec<String>,
    ) {
        self.session_id = Some(session_id);
        self.set_workspace(cwd, skill_names);
        self.blocks.clear();
        self.block_ids.clear();
        self.queued_prompts.clear();
        self.todo_items.clear();
        self.todo_dock_open = false;
        self.inspector = InspectorState::default();
        self.search = None;
        self.slash_suggestions = None;
        self.working = false;
        self.active_run_id = None;
        self.latest_run_assistant = None;
        self.terminal_tail = None;
        self.thinking_open = false;
        self.snapshot_resync_needed = false;
        self.run_started_ms = None;
        self.activity = None;
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.input_tokens_overflowed = false;
        self.output_tokens_overflowed = false;
        self.context_tokens = 0;
        self.context_window_tokens = 0;
        self.context_exact = false;
        self.compaction_status = slim_core::context::CompactionStatus::Idle;
        self.context_run_id = None;
        self.context_request_id = None;
        self.stream_output_chars = 0;
        self.request_context_base_tokens = 0;
        self.request_usage = None;
        self.request_usage_finalized = false;
        self.request_usage_overflowed = false;
        self.request_estimate_open = false;
        self.provider_timings = ProviderTimingState::default();
        self.last_tok_per_sec = None;
        self.last_tok_per_sec_estimated = false;
        self.turns_used = 0;
        self.turn_budget_warned = false;
        self.reset_this_turn_tool_budget();
        self.scroll = ScrollState::default();

        let mut saw_user = false;
        for message in messages {
            let id = self.fresh_id(match message.role {
                TranscriptRole::User => "user",
                TranscriptRole::Assistant => "assistant",
                TranscriptRole::Tool { .. } => "tool",
            });
            let mut block = match message.role {
                TranscriptRole::User => {
                    let mut block =
                        Block::new(id, BlockKind::User(message.text), BlockLifecycle::Complete);
                    if saw_user {
                        block.set_turn_boundary_before(true);
                    }
                    saw_user = true;
                    block
                }
                TranscriptRole::Assistant => Block::new(
                    id,
                    BlockKind::Assistant(message.text),
                    BlockLifecycle::Complete,
                ),
                TranscriptRole::Tool {
                    batch_id,
                    call_id,
                    name,
                    arguments,
                } => Block::new(
                    id,
                    BlockKind::Tool(ToolState {
                        historical: true,
                        batch_id,
                        call_id,
                        name,
                        materialized_output: format!(
                            "Arguments:\n{arguments}\n\nResult (saved):\n{}",
                            message.text
                        ),
                        ..ToolState::default()
                    }),
                    BlockLifecycle::Complete,
                ),
            };
            block.fold = if matches!(block.kind(), BlockKind::Tool(_)) {
                FoldState::Collapsed
            } else {
                FoldState::Auto
            };
            self.blocks.push(block);
        }
        self.revisions.content += 1;
        self.revisions.status += 1;
        self.revisions.viewport += 1;
    }

    pub fn push_notification(&mut self, message: String) {
        self.prune_notifications();
        if self.notifications.len() == 100 {
            self.notifications.remove(0);
        }
        self.notifications.push(Notification {
            message,
            created_ms: self.clock.elapsed_ms,
        });
    }

    fn maybe_warn_tool_budget(&mut self) {
        if self.tool_budget_warned {
            return;
        }
        let read_threshold = ((self.max_read_tool_calls as f64) * 0.8).ceil() as usize;
        let mutating_threshold = ((self.max_mutating_tool_calls as f64) * 0.8).ceil() as usize;
        let read_hit = self.max_read_tool_calls > 0 && self.tools_used_read >= read_threshold;
        let mutating_hit =
            self.max_mutating_tool_calls > 0 && self.tools_used_mutating >= mutating_threshold;
        if read_hit || mutating_hit {
            self.tool_budget_warned = true;
            self.push_notification(format!(
                "Tool budget: read {}/{}, mutating {}/{} — approaching this-turn limit",
                self.tools_used_read,
                self.max_read_tool_calls,
                self.tools_used_mutating,
                self.max_mutating_tool_calls
            ));
        }
    }

    fn maybe_warn_turn_budget(&mut self) {
        if self.turn_budget_warned || self.max_turns == 0 {
            return;
        }
        let threshold = ((self.max_turns as f64) * 0.8).ceil() as usize;
        if self.turns_used >= threshold {
            self.turn_budget_warned = true;
            self.push_notification(format!(
                "Turn budget: {}/{} — approaching run limit",
                self.turns_used, self.max_turns
            ));
        }
    }

    fn reset_this_turn_tool_budget(&mut self) {
        self.tools_used_read = 0;
        self.tools_used_mutating = 0;
        self.tool_budget_warned = false;
    }

    pub fn prune_notifications(&mut self) {
        let now = self.clock.elapsed_ms;
        self.notifications
            .retain(|notification| notification.is_visible(now));
    }

    pub fn visible_notifications(&self) -> impl Iterator<Item = &Notification> {
        let now = self.clock.elapsed_ms;
        self.notifications
            .iter()
            .filter(move |notification| notification.is_visible(now))
    }

    pub fn visible_toast_tail(&self, limit: usize) -> Vec<&Notification> {
        let mut notices: Vec<&Notification> = self.visible_notifications().collect();
        let start = notices.len().saturating_sub(limit);
        notices.drain(..start);
        notices
    }

    fn dismiss_notifications_starting_with(&mut self, prefix: &str) {
        self.notifications
            .retain(|notification| !notification.message.starts_with(prefix));
    }

    pub fn snapshot_resync_needed(&self) -> bool {
        self.snapshot_resync_needed
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn pending_interaction(&self) -> Option<&InteractionRequestState> {
        self.blocks.iter().rev().find_map(|block| {
            (block.lifecycle == BlockLifecycle::Pending)
                .then(|| match block.kind() {
                    BlockKind::InteractionRequest(state) if state.acknowledgement.is_none() => {
                        Some(state)
                    }
                    _ => None,
                })
                .flatten()
        })
    }

    pub(crate) fn mark_interaction_response_pending(
        &mut self,
        request_id: &InteractionRequestId,
    ) -> bool {
        let changed = self
            .blocks
            .iter_mut()
            .rev()
            .find(|block| {
                block.lifecycle == BlockLifecycle::Pending
                    && matches!(block.kind(), BlockKind::InteractionRequest(state)
                        if &state.request_id == request_id && state.acknowledgement.is_none())
            })
            .is_some_and(Block::mark_interaction_response_pending);
        if changed {
            self.revisions.content += 1;
            self.revisions.status += 1;
        }
        changed
    }

    pub(crate) fn move_question_selection(
        &mut self,
        request_id: &InteractionRequestId,
        forward: bool,
    ) -> bool {
        let changed = self
            .blocks
            .iter_mut()
            .rev()
            .find(|block| {
                matches!(block.kind(), BlockKind::InteractionRequest(state)
                if &state.request_id == request_id && state.acknowledgement.is_none())
            })
            .is_some_and(|block| block.move_question_selection(forward));
        if changed {
            self.revisions.content += 1;
        }
        changed
    }

    pub(crate) fn select_question_option(
        &mut self,
        request_id: &InteractionRequestId,
        index: usize,
    ) -> bool {
        let changed = self
            .blocks
            .iter_mut()
            .rev()
            .find(|block| {
                matches!(block.kind(), BlockKind::InteractionRequest(state)
                if &state.request_id == request_id && state.acknowledgement.is_none())
            })
            .is_some_and(|block| block.select_question_option(index));
        if changed {
            self.revisions.content += 1;
        }
        changed
    }

    pub(crate) fn activate_custom_question_answer(
        &mut self,
        request_id: &InteractionRequestId,
    ) -> bool {
        let changed = self
            .blocks
            .iter_mut()
            .rev()
            .find(|block| {
                matches!(block.kind(), BlockKind::InteractionRequest(state)
                if &state.request_id == request_id && state.acknowledgement.is_none())
            })
            .is_some_and(Block::activate_custom_question_answer);
        if changed {
            self.revisions.content += 1;
        }
        changed
    }

    pub(crate) fn record_question_answer(
        &mut self,
        request_id: &InteractionRequestId,
        answer: String,
    ) {
        let changed = self
            .blocks
            .iter_mut()
            .rev()
            .find(|block| {
                matches!(block.kind(), BlockKind::InteractionRequest(state)
                if &state.request_id == request_id)
            })
            .is_some_and(|block| block.record_question_answer(answer));
        if changed {
            self.revisions.content += 1;
        }
    }

    /// The foldable block addressed by the stable scroll anchor. Live edge
    /// intentionally has no focused block so Enter keeps its composer role.
    pub fn selected_block_id(&self) -> Option<&BlockId> {
        let id = match &self.scroll.mode {
            FollowMode::Pinned(anchor) => &anchor.block_id,
            FollowMode::Top => &self.blocks.first()?.id,
            FollowMode::LiveEdge { .. } => return None,
        };
        let index = self.blocks.iter().position(|block| &block.id == id)?;
        let block = &self.blocks[index];
        let foldable = match block.kind() {
            BlockKind::Thinking(_) => true,
            BlockKind::Tool(tool) => {
                tool.content_handle.is_some()
                    || crate::block::consecutive_complete_tool_span(&self.blocks, index)
                        .is_some_and(|(start, end)| {
                            crate::block::complete_tool_count(&self.blocks, start, end) > 1
                        })
                    || crate::block::consecutive_identical_failed_tool_span(&self.blocks, index)
                        .is_some_and(|(start, end)| end.saturating_sub(start) > 1)
            }
            _ => false,
        };
        foldable.then_some(&block.id)
    }

    pub fn toggle_block(&mut self, id: &BlockId) -> bool {
        let Some(block) = self
            .blocks
            .iter_mut()
            .find(|block| &block.id == id && matches!(block.kind(), BlockKind::Thinking(_)))
        else {
            return false;
        };
        block.fold = match block.fold {
            FoldState::Expanded => FoldState::Collapsed,
            FoldState::Auto | FoldState::Collapsed => FoldState::Expanded,
        };
        if let FollowMode::Pinned(anchor) = &mut self.scroll.mode {
            if &anchor.block_id == id {
                anchor.row_offset = 0;
            }
        }
        self.revisions.fold += 1;
        true
    }

    /// Activates a foldable block. Thinking toggles synchronously; tool output
    /// pages are requested causally and materialized only by a matching event.
    pub fn activate_block(&mut self, id: &BlockId) -> (bool, Option<UiCommand>) {
        let Some(index) = self.blocks.iter().position(|block| &block.id == id) else {
            return (false, None);
        };
        if matches!(self.blocks[index].kind(), BlockKind::Thinking(_)) {
            let target = crate::block::consecutive_complete_thinking_span(&self.blocks, index)
                .filter(|(start, end)| end.saturating_sub(*start) > 1)
                .map(|(start, _)| self.blocks[start].id.clone())
                .unwrap_or_else(|| id.clone());
            return (self.toggle_block(&target), None);
        }

        let BlockKind::Tool(tool) = self.blocks[index].kind() else {
            return (false, None);
        };
        let grouped = crate::block::consecutive_complete_tool_span(&self.blocks, index)
            .filter(|(start, end)| {
                crate::block::complete_tool_count(&self.blocks, *start, *end) > 1
            })
            .or_else(|| {
                crate::block::consecutive_identical_failed_tool_span(&self.blocks, index)
                    .filter(|(start, end)| end.saturating_sub(*start) > 1)
            });
        if let Some((start, _end)) = grouped {
            let leader_id = self.blocks[start].id.clone();
            self.blocks[start].fold = match self.blocks[start].fold {
                FoldState::Expanded => FoldState::Collapsed,
                FoldState::Auto | FoldState::Collapsed => FoldState::Expanded,
            };
            if let FollowMode::Pinned(anchor) = &mut self.scroll.mode {
                anchor.block_id = leader_id;
                anchor.row_offset = 0;
            }
            self.revisions.fold += 1;
            return (true, None);
        }
        let was_expanded = self.blocks[index].fold == FoldState::Expanded;
        let has_materialized_output = !tool.materialized_output.is_empty();
        let pending = tool.pending_page.is_some();
        let next_cursor = tool.next_cursor;
        let handle = tool.content_handle.clone();

        if !was_expanded {
            self.blocks[index].fold = FoldState::Expanded;
            self.revisions.fold += 1;
            if has_materialized_output || pending || handle.is_none() {
                return (true, None);
            }
        } else if pending {
            return (false, None);
        } else if (has_materialized_output && next_cursor.is_none()) || handle.is_none() {
            self.blocks[index].fold = FoldState::Collapsed;
            self.revisions.fold += 1;
            return (true, None);
        }

        let Some(next_request_id) = self.next_content_request_id.checked_add(1) else {
            self.push_notification("Content request identity exhausted".into());
            self.revisions.status += 1;
            return (true, None);
        };
        let request_id = ContentRequestId(self.next_content_request_id);
        self.next_content_request_id = next_request_id;
        let cursor = if has_materialized_output {
            next_cursor
        } else {
            None
        };
        let tool = self.blocks[index]
            .tool_state_mut()
            .expect("tool checked above");
        tool.pending_page = Some(PendingContentPage { request_id, cursor });
        let command = UiCommand::RequestContentPage {
            handle: handle.expect("tool handle checked above"),
            request_id,
            cursor,
        };
        (true, Some(command))
    }

    /// Append-only public boundary. Duplicate IDs are rejected so a caller
    /// cannot replace cached content while reusing its generation key.
    pub fn append_block(&mut self, mut block: Block) -> bool {
        if !self.block_ids.insert(block.id.0.clone()) {
            return false;
        }
        if matches!(block.kind(), BlockKind::User(_))
            && self
                .blocks
                .iter()
                .any(|existing| matches!(existing.kind(), BlockKind::User(_)))
        {
            block.set_turn_boundary_before(true);
        }
        let prompt_id = matches!(block.kind(), BlockKind::User(_)).then(|| block.id.clone());
        self.blocks.push(block);
        self.note_new_content();
        if self.scroll.is_live_edge() {
            if let Some(prompt_id) = prompt_id {
                self.scroll.mode = FollowMode::LiveEdge {
                    prompt_id: Some(prompt_id),
                };
            }
        }
        self.revisions.content += 1;
        true
    }

    /// Stable monotonic block ids (spec §2 "blocos tipados com IDs e revisions
    /// estáveis"); index-based ids collided after removals.
    fn fresh_id(&mut self, kind: &str) -> String {
        loop {
            let id = format!("{kind}-{}", self.next_block_id);
            self.next_block_id += 1;
            if self.block_ids.insert(Arc::from(id.as_str())) {
                return id;
            }
        }
    }

    /// Enqueues a prompt while a run is active (§7.4): visible `QueuedUser`
    /// block in FIFO order; drained one-per-turn at run boundaries.
    pub(crate) fn enqueue_queued_prompt(&mut self, text: String) {
        let position = self.queued_prompts.len();
        let id = self.fresh_id("queued");
        let block = Block::new(
            id,
            BlockKind::QueuedUser(format!("queued[{position}] {text}")),
            BlockLifecycle::Complete,
        );
        self.blocks.push(block);
        self.queued_prompts.push_back(text);
        self.note_new_content();
        self.revisions.content += 1;
    }

    /// Pops the next queued prompt and removes its `QueuedUser` block.
    pub(crate) fn pop_queued_prompt(&mut self) -> Option<String> {
        let prompt = self.queued_prompts.pop_front()?;
        // Blocks are pushed in enqueue order, so the first remaining QueuedUser
        // block in `self.blocks` is the FIFO head (no reordering of blocks).
        // Removed the previous `kind_text()` probe: Block has no such accessor.
        if let Some(index) = self
            .blocks
            .iter()
            .position(|block| matches!(block.kind(), BlockKind::QueuedUser(_)))
        {
            let block = self.blocks.remove(index);
            self.block_ids.remove(&block.id.0);
            self.revisions.content += 1;
        }
        Some(prompt)
    }

    fn transition_activity(&mut self, phase: ActivityPhase) {
        if self.activity.as_ref().map(|activity| &activity.phase) == Some(&phase) {
            return;
        }
        self.activity = Some(ActivityState {
            phase,
            started_ms: self.clock.elapsed_ms,
        });
        self.revisions.status += 1;
    }

    fn apply_interaction_request(&mut self, request: InteractionRequestState) {
        if let Some(existing) = self.blocks.iter().find_map(|block| match block.kind() {
            BlockKind::InteractionRequest(existing)
                if existing.request_id == request.request_id =>
            {
                Some(existing)
            }
            _ => None,
        }) {
            if existing.same_request(&request) {
                return;
            }
            self.snapshot_resync_needed = true;
            self.push_notification("Conflicting interaction request identity ignored".into());
            self.revisions.status += 1;
            return;
        }

        let id = self.fresh_id("interaction");
        self.blocks.push(Block::new(
            id,
            BlockKind::InteractionRequest(request),
            BlockLifecycle::Pending,
        ));
        self.slash_suggestions = None;
        self.note_new_content();
        if self.terminal_tail.is_none() {
            self.transition_activity(ActivityPhase::WaitingForInput);
        }
        self.revisions.content += 1;
    }

    fn acknowledge_interaction(
        &mut self,
        request_id: &InteractionRequestId,
        accepted: bool,
        message: String,
    ) {
        let acknowledged = self
            .blocks
            .iter_mut()
            .rev()
            .find(|block| {
                block.lifecycle == BlockLifecycle::Pending
                    && matches!(block.kind(), BlockKind::InteractionRequest(state)
                        if &state.request_id == request_id && state.acknowledgement.is_none())
            })
            .is_some_and(|block| block.acknowledge_interaction(accepted, message));
        if !acknowledged {
            return;
        }

        if self.pending_interaction().is_some() {
            if self.terminal_tail.is_none() {
                self.transition_activity(ActivityPhase::WaitingForInput);
            }
        } else if self.working && self.terminal_tail.is_none() {
            self.transition_activity(ActivityPhase::AwaitingProvider);
        } else {
            self.activity = None;
        }
        self.revisions.content += 1;
        self.revisions.status += 1;
    }

    fn apply_usage_estimate(
        &mut self,
        run_id: Option<u64>,
        request_id: u64,
        context_tokens: u64,
        context_window_tokens: u64,
    ) {
        if let Some(incoming_run_id) = run_id {
            let stale_terminal = self
                .terminal_tail
                .is_some_and(|terminal| terminal.run_id >= incoming_run_id);
            let older_than_active = self
                .active_run_id
                .is_some_and(|active_run_id| incoming_run_id < active_run_id);
            let older_than_context = self
                .context_run_id
                .is_some_and(|context_run_id| incoming_run_id < context_run_id);
            let stale_request = self.context_run_id == run_id
                && self
                    .context_request_id
                    .is_some_and(|current| request_id < current);
            let conflicting_identity = self.context_run_id == run_id
                && self.context_request_id == Some(request_id)
                && self.context_window_tokens != 0
                && self.context_window_tokens != context_window_tokens;
            if stale_terminal
                || older_than_active
                || older_than_context
                || stale_request
                || conflicting_identity
            {
                return;
            }
        } else if self.context_window_tokens == context_window_tokens
            && (self.request_estimate_open
                || self
                    .context_request_id
                    .is_some_and(|current| request_id <= current))
        {
            // Legacy providerless snapshots may seed the next idle request,
            // but cannot replace an active/newer correlated identity.
            return;
        }
        let same_identity = self.request_estimate_open
            && self.context_run_id == run_id
            && self.context_request_id == Some(request_id)
            && self.context_window_tokens == context_window_tokens;
        self.context_run_id = run_id;
        self.context_request_id = Some(request_id);
        self.context_window_tokens = context_window_tokens;
        if same_identity {
            self.context_tokens = self.context_tokens.max(context_tokens);
            self.request_context_base_tokens = self.request_context_base_tokens.max(context_tokens);
        } else {
            self.context_tokens = context_tokens;
            self.request_context_base_tokens = context_tokens;
            self.request_estimate_open = true;
            self.last_tok_per_sec = None;
            self.last_tok_per_sec_estimated = false;
            self.provider_timings.first_semantic_ms = None;
            if run_id.is_some() {
                self.turns_used = self.turns_used.saturating_add(1);
                self.reset_this_turn_tool_budget();
                self.maybe_warn_turn_budget();
            }
        }
        self.context_exact = false;
        self.stream_output_chars = 0;
        self.request_usage = None;
        self.request_usage_finalized = false;
        self.request_usage_overflowed = false;
        self.revisions.status += 1;
    }

    fn note_request_usage(&mut self, input_tokens: u64, output_tokens: u64, finalized: bool) {
        let (total_input, input_total_overflowed) = self.input_tokens.overflowing_add(input_tokens);
        let (total_output, output_total_overflowed) =
            self.output_tokens.overflowing_add(output_tokens);
        self.input_tokens = if input_total_overflowed {
            u64::MAX
        } else {
            total_input
        };
        self.output_tokens = if output_total_overflowed {
            u64::MAX
        } else {
            total_output
        };
        self.input_tokens_overflowed |= input_total_overflowed;
        self.output_tokens_overflowed |= output_total_overflowed;
        let (request_input, request_output) = self.request_usage.unwrap_or((0, 0));
        let (request_input, input_overflowed) = request_input.overflowing_add(input_tokens);
        let (request_output, output_overflowed) = request_output.overflowing_add(output_tokens);
        self.request_usage_overflowed |= input_overflowed || output_overflowed;
        self.request_usage = Some((
            if input_overflowed {
                u64::MAX
            } else {
                request_input
            },
            if output_overflowed {
                u64::MAX
            } else {
                request_output
            },
        ));
        // Provider normalization emits terminal Usage only for complete
        // accounting (including an additive-zero Anthropic marker).
        self.request_usage_finalized |= finalized;
        self.revisions.status += 1;
    }

    fn project_request_usage(&mut self) {
        if !self.request_usage_finalized {
            return;
        }
        if let Some((input_tokens, output_tokens)) = self.request_usage {
            let (context_tokens, overflowed) = input_tokens.overflowing_add(output_tokens);
            self.request_usage_overflowed |= overflowed;
            self.context_tokens = if overflowed { u64::MAX } else { context_tokens };
            self.request_context_base_tokens = self.context_tokens;
            self.context_exact = false;
            self.stream_output_chars = 0;
            self.revisions.status += 1;
        }
    }

    fn close_request_usage(&mut self, successful: bool) {
        let had_current_request = self.request_estimate_open || self.request_usage.is_some();
        self.project_request_usage();
        if successful && self.request_usage_finalized && !self.request_usage_overflowed {
            self.context_exact = true;
        } else if had_current_request {
            self.context_exact = false;
        }
        self.request_usage = None;
        self.request_usage_finalized = false;
        self.request_usage_overflowed = false;
        self.request_estimate_open = false;
    }

    fn note_stream_output_chars(&mut self, chars: u64) {
        if chars == 0 {
            return;
        }
        self.stream_output_chars = self.stream_output_chars.saturating_add(chars);
        let estimate = self.request_context_base_tokens.saturating_add(
            slim_core::context::estimate_text_tokens_from_chars(self.stream_output_chars),
        );
        if estimate > self.context_tokens || self.context_exact {
            self.context_tokens = self.context_tokens.max(estimate);
            self.context_exact = false;
            self.revisions.status += 1;
        }
    }

    fn recalculate_tok_per_sec(&mut self, provider_latency_ms: u64) {
        self.last_tok_per_sec = None;
        self.last_tok_per_sec_estimated = false;
        self.revisions.status += 1;
        let Some(ms) = self
            .provider_timings
            .first_semantic_ms
            .and_then(|start| provider_latency_ms.checked_sub(start))
            .filter(|ms| *ms > 0)
        else {
            return;
        };
        if self.request_usage_overflowed {
            return;
        }
        let estimated = !self.request_usage_finalized;
        let output_tokens = self
            .request_usage
            .filter(|_| !estimated)
            .map(|(_, out)| out)
            .unwrap_or_else(|| {
                slim_core::context::estimate_text_tokens_from_chars(self.stream_output_chars)
            });
        if output_tokens > 0 {
            let tenths = (u128::from(output_tokens) * 10_000 / u128::from(ms))
                .min(u128::from(u64::MAX)) as u64;
            self.last_tok_per_sec = Some(tenths);
            self.last_tok_per_sec_estimated = estimated;
        }
    }

    fn terminalize_streaming(&mut self, lifecycle: BlockLifecycle) {
        // Lifecycle changes are content changes for the height/selection memos;
        // callers bump too, but the invariant lives here so a future caller
        // cannot forget it.
        self.revisions.content += 1;
        for block in &mut self.blocks {
            if block.lifecycle == BlockLifecycle::Streaming
                && matches!(
                    block.kind(),
                    BlockKind::Assistant(_) | BlockKind::Thinking(_) | BlockKind::Tool(_)
                )
            {
                block.lifecycle = lifecycle;
            }
        }
    }

    fn terminalize_latest_assistant(&mut self, lifecycle: BlockLifecycle) {
        let Some(id) = self.latest_run_assistant.take() else {
            return;
        };
        self.revisions.content += 1;
        if let Some(block) = self.blocks.iter_mut().find(|block| {
            block.id == id
                && matches!(
                    block.lifecycle,
                    BlockLifecycle::Streaming | BlockLifecycle::Complete
                )
        }) {
            block.lifecycle = lifecycle;
        }
    }

    fn accept_terminal(&mut self, run_id: u64, lifecycle: BlockLifecycle) -> bool {
        if self.active_run_id.is_some_and(|active| active != run_id) {
            return false;
        }
        if self
            .terminal_tail
            .is_some_and(|terminal| terminal.run_id >= run_id)
        {
            // The first accepted terminal for a run is authoritative. Later
            // equal-ID outcomes are duplicates/conflicts and cannot rewrite it.
            return false;
        }
        self.active_run_id = None;
        self.terminal_tail = Some(TerminalTail { run_id, lifecycle });
        true
    }

    fn terminal_tail_lifecycle(&self) -> Option<BlockLifecycle> {
        self.terminal_tail.map(|terminal| terminal.lifecycle)
    }

    /// New content while the user is pinned away from live edge increments the
    /// unseen counter (spec §13.3).
    fn note_new_content(&mut self) {
        if self.scroll.is_pinned() {
            self.scroll.unseen = self.scroll.unseen.saturating_add(1);
        }
    }

    pub fn apply_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::SessionSnapshot {
                session_id,
                cwd,
                skill_names,
            } => self.apply_snapshot(session_id, cwd, skill_names),
            UiEvent::SessionRestored {
                session_id,
                cwd,
                messages,
                skill_names,
            } => self.restore_session(session_id, cwd, messages, skill_names),
            UiEvent::WorkspaceChanged { cwd, skill_names } => {
                self.set_workspace(cwd, skill_names);
                self.revisions.status += 1;
            }
            UiEvent::AttachmentsChanged { labels } => {
                self.attachment_labels = labels;
                self.revisions.content += 1;
            }
            UiEvent::RunStarted {
                run_id,
                max_mutating_tool_calls,
                max_read_tool_calls,
                max_turns,
            } => {
                if self
                    .terminal_tail
                    .is_some_and(|terminal| terminal.run_id >= run_id)
                {
                    // This start was already overtaken by its expedited
                    // terminal, or is older than the latest terminal. Preserve
                    // the newest truthful tail outcome.
                } else {
                    // A genuine new run is an output boundary even if an
                    // upstream terminal was lost.
                    self.terminalize_streaming(BlockLifecycle::Cancelled);
                    self.terminal_tail = None;
                    self.thinking_open = false;
                    self.active_run_id = Some(run_id);
                    self.latest_run_assistant = None;
                    self.working = true;
                    self.run_started_ms = Some(self.clock.elapsed_ms);
                    self.activity = None;
                    self.last_tok_per_sec = None;
                    self.last_tok_per_sec_estimated = false;
                    self.max_mutating_tool_calls = max_mutating_tool_calls;
                    self.max_read_tool_calls = max_read_tool_calls;
                    self.max_turns = max_turns;
                    // ContextSnapshot owns request-accounting reset and may
                    // arrive on control before this ordered RunStarted. Only
                    // clear stale accounting when no fresh snapshot is open.
                    let stale_other_run = self
                        .context_run_id
                        .is_some_and(|context_run_id| context_run_id != run_id)
                        && !self.context_exact;
                    if stale_other_run {
                        self.context_tokens = 0;
                        self.context_window_tokens = 0;
                        self.request_context_base_tokens = 0;
                        self.context_run_id = None;
                        self.context_request_id = None;
                        self.request_estimate_open = false;
                    }
                    let fresh_snapshot = self.request_estimate_open
                        && self
                            .context_run_id
                            .is_none_or(|context_run_id| context_run_id == run_id);
                    if !fresh_snapshot {
                        self.stream_output_chars = 0;
                        self.request_usage = None;
                        self.request_usage_finalized = false;
                        self.request_usage_overflowed = false;
                        self.request_estimate_open = false;
                        self.turns_used = 0;
                        self.turn_budget_warned = false;
                        self.reset_this_turn_tool_budget();
                    }
                    self.revisions.status += 1;
                }
            }
            UiEvent::RunCompleted { run_id } => {
                if self.accept_terminal(run_id, BlockLifecycle::Complete) {
                    self.terminalize_streaming(BlockLifecycle::Complete);
                    self.latest_run_assistant = None;
                    self.close_request_usage(true);
                    let pending = self
                        .todo_items
                        .iter()
                        .filter(|item| {
                            !matches!(
                                item.status,
                                crate::api::TodoItemStatus::Completed
                                    | crate::api::TodoItemStatus::Cancelled
                            )
                        })
                        .count();
                    if pending > 0 {
                        let id = self.fresh_id("pending-tasks");
                        self.blocks.push(Block::new(
                            id,
                            BlockKind::System(format!(
                                "Run ended; {pending} recorded task(s) remain pending."
                            )),
                            BlockLifecycle::Complete,
                        ));
                        self.note_new_content();
                    }
                    self.working = false;
                    self.thinking_open = false;
                    self.run_started_ms = None;
                    self.activity = None;
                    self.request_estimate_open = false;
                    self.revisions.content += 1;
                    self.revisions.status += 1;
                }
            }
            UiEvent::RunStopped { run_id, message } => {
                if self.accept_terminal(run_id, BlockLifecycle::Cancelled) {
                    self.terminalize_streaming(BlockLifecycle::Cancelled);
                    self.terminalize_latest_assistant(BlockLifecycle::Cancelled);
                    self.close_request_usage(false);
                    self.working = false;
                    self.thinking_open = false;
                    self.run_started_ms = None;
                    self.activity = None;
                    self.request_estimate_open = false;
                    let id = self.fresh_id("stop");
                    let mut block =
                        Block::new(id, BlockKind::System(message), BlockLifecycle::Complete);
                    block.fold = FoldState::Collapsed;
                    self.blocks.push(block);
                    self.note_new_content();
                    self.turns_used = 0;
                    self.turn_budget_warned = false;
                    self.reset_this_turn_tool_budget();
                    self.revisions.content += 1;
                    self.revisions.status += 1;
                }
            }
            UiEvent::RunCancelled { run_id } => {
                if self.accept_terminal(run_id, BlockLifecycle::Cancelled) {
                    self.working = false;
                    self.thinking_open = false;
                    self.run_started_ms = None;
                    self.activity = None;
                    self.terminalize_streaming(BlockLifecycle::Cancelled);
                    self.terminalize_latest_assistant(BlockLifecycle::Cancelled);
                    self.close_request_usage(false);
                    self.push_notification("run cancelled".into());
                    self.revisions.content += 1;
                    self.revisions.status += 1;
                }
            }
            UiEvent::RunFailed { run_id, message } => {
                let terminal_run_id = run_id.or(self.active_run_id);
                let accepted = terminal_run_id
                    .is_none_or(|run_id| self.accept_terminal(run_id, BlockLifecycle::Failed));
                if accepted {
                    self.terminalize_streaming(BlockLifecycle::Failed);
                    self.terminalize_latest_assistant(BlockLifecycle::Failed);
                    self.close_request_usage(false);
                    self.working = false;
                    self.thinking_open = false;
                    self.run_started_ms = None;
                    self.activity = None;
                    let id = self.fresh_id("error");
                    self.blocks.push(Block::new(
                        id,
                        BlockKind::Error(message),
                        BlockLifecycle::Failed,
                    ));
                    self.note_new_content();
                    self.revisions.content += 1;
                    self.revisions.status += 1;
                }
            }
            UiEvent::UserMessageAdded { text } => {
                let id = self.fresh_id("user");
                let mut block = Block::new(id, BlockKind::User(text), BlockLifecycle::Complete);
                if self
                    .blocks
                    .iter()
                    .any(|existing| matches!(existing.kind(), BlockKind::User(_)))
                {
                    block.set_turn_boundary_before(true);
                }
                let prompt_id = block.id.clone();
                self.blocks.push(block);
                self.note_new_content();
                if self.scroll.is_live_edge() {
                    self.scroll.mode = FollowMode::LiveEdge {
                        prompt_id: Some(prompt_id),
                    };
                }
                self.revisions.content += 1;
            }
            UiEvent::RestoreDraft { text } => {
                if self.composer.is_empty() {
                    self.composer.insert_text(text);
                    self.revisions.content += 1;
                }
            }
            UiEvent::AssistantDelta { text } => {
                let output_chars = text.chars().count() as u64;
                // RunStarted is the authority for working state; buffered
                // deltas after a terminal preserve its truthful outcome without
                // reviving activity or a streaming lifecycle.
                let terminal_tail = self.terminal_tail_lifecycle();
                if terminal_tail.is_none() {
                    // The streaming redactor may retain a possible secret
                    // prefix until the provider closes the turn. If a tool has
                    // already been announced, that safe tail still belongs to
                    // the current assistant block and must not bounce the
                    // activity back from "Preparing tool" to "Responding".
                    let preparing_tool = matches!(
                        self.activity.as_ref().map(|activity| &activity.phase),
                        Some(ActivityPhase::External(label))
                            if label == "Preparing tool"
                                || label.starts_with("Preparing tool ·")
                    );
                    if !preparing_tool {
                        self.transition_activity(ActivityPhase::Responding);
                    }
                    self.note_stream_output_chars(output_chars);
                }
                let lifecycle = terminal_tail.unwrap_or(BlockLifecycle::Streaming);
                if let Some(block) = self.blocks.last_mut().filter(|block| {
                    (block.lifecycle == BlockLifecycle::Streaming
                        || terminal_tail == Some(block.lifecycle))
                        && matches!(block.kind(), BlockKind::Assistant(_))
                }) {
                    block.append_text(&text);
                    block.lifecycle = lifecycle;
                } else {
                    let id = self.fresh_id("assistant");
                    self.blocks
                        .push(Block::new(id, BlockKind::Assistant(text), lifecycle));
                }
                self.latest_run_assistant = self.blocks.last().map(|block| block.id.clone());
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::AssistantEnded => {
                // The provider's accounting fence precedes the run outcome.
                // Project the actual count as approximate now; only
                // RunCompleted may promote it to exact.
                self.project_request_usage();
                self.request_estimate_open = false;
                if let Some(block) = self.blocks.iter_mut().rev().find(|block| {
                    block.lifecycle == BlockLifecycle::Streaming
                        && matches!(block.kind(), BlockKind::Assistant(_))
                }) {
                    block.lifecycle = BlockLifecycle::Complete;
                }
                self.revisions.content += 1;
            }
            UiEvent::ThinkingStarted => {
                if self.terminal_tail.is_none() {
                    self.thinking_open = true;
                    self.transition_activity(ActivityPhase::Thinking);
                }
            }
            UiEvent::ThinkingDelta { text } => {
                if text.trim().is_empty() {
                    let terminal_tail = self.terminal_tail_lifecycle();
                    let would_append = self.blocks.last().is_some_and(|block| {
                        (block.lifecycle == BlockLifecycle::Streaming
                            || terminal_tail == Some(block.lifecycle))
                            && matches!(block.kind(), BlockKind::Thinking(_))
                    });
                    if !would_append {
                        return;
                    }
                }
                let output_chars = text.chars().count() as u64;
                let terminal_tail = self.terminal_tail_lifecycle();
                if terminal_tail.is_none() && !self.thinking_open {
                    if !self.snapshot_resync_needed {
                        self.snapshot_resync_needed = true;
                        let message =
                            "reasoning stream gap: delta arrived before start; snapshot resync required";
                        let id = self.fresh_id("reasoning-gap");
                        self.blocks.push(Block::new(
                            id,
                            BlockKind::Error(message.into()),
                            BlockLifecycle::Failed,
                        ));
                        self.push_notification(message.into());
                        self.note_new_content();
                        self.revisions.content += 1;
                        self.revisions.status += 1;
                    }
                    return;
                }
                if terminal_tail.is_none() {
                    self.transition_activity(ActivityPhase::Thinking);
                    self.note_stream_output_chars(output_chars);
                }
                let lifecycle = terminal_tail.unwrap_or(BlockLifecycle::Streaming);
                if let Some(block) = self.blocks.last_mut().filter(|block| {
                    (block.lifecycle == BlockLifecycle::Streaming
                        || terminal_tail == Some(block.lifecycle))
                        && matches!(block.kind(), BlockKind::Thinking(_))
                }) {
                    block.append_text(&text);
                    block.lifecycle = lifecycle;
                } else {
                    let mut block = Block::new(
                        self.fresh_id("thinking"),
                        BlockKind::Thinking(text),
                        lifecycle,
                    );
                    block.fold = FoldState::Collapsed;
                    self.blocks.push(block);
                }
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::ThinkingEnded => {
                let lifecycle_was_open = std::mem::take(&mut self.thinking_open);
                if let Some(block) = self.blocks.iter_mut().rev().find(|block| {
                    block.lifecycle == BlockLifecycle::Streaming
                        && matches!(block.kind(), BlockKind::Thinking(_))
                }) {
                    block.lifecycle = BlockLifecycle::Complete;
                }
                self.blocks.retain(|block| {
                    if let BlockKind::Thinking(text) = block.kind() {
                        !text.trim().is_empty()
                    } else {
                        true
                    }
                });
                let was_thinking = matches!(
                    self.activity.as_ref().map(|activity| &activity.phase),
                    Some(ActivityPhase::Thinking)
                );
                if lifecycle_was_open
                    && was_thinking
                    && self.working
                    && self.terminal_tail.is_none()
                {
                    self.transition_activity(ActivityPhase::AwaitingProvider);
                }
                self.revisions.content += 1;
            }
            UiEvent::UsageEstimate {
                request_id,
                context_tokens,
                context_window_tokens,
            } => self.apply_usage_estimate(None, request_id, context_tokens, context_window_tokens),
            UiEvent::UsageEstimateForRun {
                run_id,
                request_id,
                context_tokens,
                context_window_tokens,
            } => self.apply_usage_estimate(
                Some(run_id),
                request_id,
                context_tokens,
                context_window_tokens,
            ),
            UiEvent::UsagePartial {
                input_tokens,
                output_tokens,
            } => self.note_request_usage(input_tokens, output_tokens, false),
            UiEvent::Usage {
                input_tokens,
                output_tokens,
            } => self.note_request_usage(input_tokens, output_tokens, true),
            UiEvent::RequestCompleted {
                provider_latency_ms,
            } => self.recalculate_tok_per_sec(provider_latency_ms),
            UiEvent::ModeChanged { mode } => {
                self.mode = mode;
                self.revisions.status += 1;
            }
            UiEvent::ModelChanged { model } => {
                self.model = model;
                self.model_overlay = None;
                self.revisions.status += 1;
            }
            UiEvent::OpenCodeCatalogLoaded { models, source } => {
                self.open_code_models = models;
                self.open_code_catalog_source = Some(source);
                self.catalog_revision += 1;
                // When the unified overlay is open, rebuild its layout so the
                // catalog items appear as soon as they arrive.
                if let Some(overlay) = self.model_overlay.as_mut() {
                    let rows = overlay.rows(
                        &self.open_code_models,
                        &self.cline_pass_models,
                        &self.command_code_models,
                        &self.zen_models,
                    );
                    overlay.selected = overlay.selected.min(rows.len().saturating_sub(1));
                }
                self.revisions.status += 1;
            }
            UiEvent::ClinePassCatalogLoaded { models, source } => {
                self.cline_pass_models = models;
                self.cline_pass_catalog_source = Some(source);
                self.catalog_revision += 1;
                if let Some(overlay) = self.model_overlay.as_mut() {
                    let rows = overlay.rows(
                        &self.open_code_models,
                        &self.cline_pass_models,
                        &self.command_code_models,
                        &self.zen_models,
                    );
                    overlay.selected = overlay.selected.min(rows.len().saturating_sub(1));
                }
                self.revisions.status += 1;
            }
            UiEvent::CommandCodeCatalogLoaded { models, source } => {
                self.command_code_models = models;
                self.command_code_catalog_source = Some(source);
                self.catalog_revision += 1;
                if let Some(overlay) = self.model_overlay.as_mut() {
                    let rows = overlay.rows(
                        &self.open_code_models,
                        &self.cline_pass_models,
                        &self.command_code_models,
                        &self.zen_models,
                    );
                    overlay.selected = overlay.selected.min(rows.len().saturating_sub(1));
                }
                self.revisions.status += 1;
            }
            UiEvent::ZenCatalogLoaded { models, source } => {
                self.zen_models = models;
                self.zen_catalog_source = Some(source);
                self.catalog_revision += 1;
                if let Some(overlay) = self.model_overlay.as_mut() {
                    let rows = overlay.rows(
                        &self.open_code_models,
                        &self.cline_pass_models,
                        &self.command_code_models,
                        &self.zen_models,
                    );
                    overlay.selected = overlay.selected.min(rows.len().saturating_sub(1));
                }
                self.revisions.status += 1;
            }
            UiEvent::McpServersChanged { servers } => {
                // Snapshots can reorder or drop entries (a server removed via
                // config or /mcp remove): keep the cursor on the same server
                // by name, not on the same numeric index.
                let selected_name = self.mcp_overlay.as_ref().and_then(|overlay| {
                    self.mcp_servers
                        .get(overlay.selected)
                        .map(|server| server.name.clone())
                });
                self.mcp_servers = servers;
                self.mcp_revision += 1;
                if let Some(overlay) = self.mcp_overlay.as_mut() {
                    overlay.selected = selected_name
                        .and_then(|name| {
                            self.mcp_servers
                                .iter()
                                .position(|server| server.name == name)
                        })
                        .unwrap_or_else(|| {
                            overlay
                                .selected
                                .min(self.mcp_servers.len().saturating_sub(1))
                        });
                    if let Some(name) = overlay.confirm_remove.as_ref() {
                        if !self.mcp_servers.iter().any(|server| &server.name == name) {
                            overlay.confirm_remove = None;
                        }
                    }
                }
                self.revisions.status += 1;
            }
            UiEvent::CodexSpeedChanged { fast } => {
                self.codex_fast = fast;
                self.revisions.status += 1;
            }
            UiEvent::EffortChanged { effort } => {
                self.effort = effort;
                self.effort_overlay = None;
                self.revisions.status += 1;
            }
            UiEvent::AuthStateChanged {
                provider,
                authenticated,
            } => {
                self.auth_provider = provider;
                self.authenticated = authenticated;
                if authenticated {
                    self.login_overlay = None;
                    self.dismiss_notifications_starting_with("No provider connected");
                } else {
                    self.dismiss_notifications_starting_with("Connected:");
                }
                self.revisions.status += 1;
            }
            UiEvent::LoginProgress { message } => {
                if let Some(overlay) = self.login_overlay.as_mut() {
                    overlay.in_progress = true;
                    overlay.progress = Some(message);
                }
                self.revisions.status += 1;
            }
            UiEvent::LoginFailed { message } => {
                // G251: a terminal failure must release the overlay — input
                // stays blocked while `in_progress`, so leaving it set would
                // freeze the login dialog on the error message.
                if let Some(overlay) = self.login_overlay.as_mut() {
                    overlay.in_progress = false;
                    overlay.progress = Some(message);
                }
                self.revisions.status += 1;
            }
            UiEvent::LoginUrl { url, user_code } => {
                if let Some(overlay) = self.login_overlay.as_mut() {
                    overlay.in_progress = true;
                    overlay.auth_url = Some(url);
                    overlay.user_code = user_code;
                }
                self.revisions.status += 1;
            }
            UiEvent::ToolStarted {
                batch_id,
                call_id,
                name,
                arguments_summary,
            } => {
                if self.blocks.iter().any(|block| {
                    matches!(block.kind(), BlockKind::Tool(state)
                        if state.batch_id == batch_id && state.call_id == call_id)
                }) {
                    self.snapshot_resync_needed = true;
                    self.push_notification("Duplicate tool lifecycle identity ignored".into());
                    self.revisions.status += 1;
                    return;
                }
                let id = self.fresh_id("tool");
                let lifecycle = self
                    .terminal_tail_lifecycle()
                    .unwrap_or(BlockLifecycle::Streaming);
                if lifecycle == BlockLifecycle::Streaming {
                    self.transition_activity(ActivityPhase::RunningTool(name.clone()));
                }
                self.blocks.push(Block::new(
                    id,
                    BlockKind::Tool(ToolState {
                        historical: false,
                        batch_id,
                        call_id,
                        name,
                        arguments_summary,
                        preview: String::new(),
                        duration_ms: None,
                        content_handle: None,
                        materialized_output: String::new(),
                        next_cursor: None,
                        pending_page: None,
                    }),
                    lifecycle,
                ));
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::ToolProgress {
                batch_id,
                call_id,
                name: _,
                preview,
                content_handle,
            } => {
                let found = if let Some(block) = self.blocks.iter_mut().find(|block| {
                    matches!(block.kind(), BlockKind::Tool(state)
                        if state.batch_id == batch_id && state.call_id == call_id)
                }) {
                    block.set_tool_preview(preview);
                    if let Some(state) = block.tool_state_mut() {
                        // Process summaries are projected through the same
                        // progress lane after ToolOutput. A missing handle
                        // means "leave the existing inspector attachment";
                        // only a newly supplied handle replaces it.
                        if content_handle.is_some() {
                            state.content_handle = content_handle;
                        }
                    }
                    true
                } else {
                    false
                };
                if !found {
                    self.snapshot_resync_needed = true;
                    self.push_notification("Orphan tool lifecycle progress ignored".into());
                    self.revisions.status += 1;
                }
                self.revisions.content += 1;
            }
            UiEvent::ToolEnded {
                batch_id,
                call_id,
                name,
                success,
                duration_ms,
            } => {
                let mut ended_name = None;
                let mut duplicate_terminal = false;
                let ended = if let Some(block) = self.blocks.iter_mut().find(|block| {
                    matches!(block.kind(), BlockKind::Tool(state)
                        if state.batch_id == batch_id && state.call_id == call_id)
                }) {
                    let transitioned = block.lifecycle == BlockLifecycle::Streaming;
                    if transitioned {
                        block.lifecycle = if success {
                            BlockLifecycle::Complete
                        } else {
                            BlockLifecycle::Failed
                        };
                    }
                    if let Some(state) = block.tool_state_mut() {
                        if state.duration_ms.is_none() {
                            state.duration_ms = Some(duration_ms);
                        } else {
                            duplicate_terminal = true;
                        }
                        ended_name = Some(state.name.clone());
                    }
                    transitioned
                } else {
                    self.snapshot_resync_needed = true;
                    self.push_notification("Orphan tool lifecycle terminal ignored".into());
                    self.revisions.status += 1;
                    false
                };
                if duplicate_terminal {
                    self.snapshot_resync_needed = true;
                    self.push_notification("Duplicate tool lifecycle terminal ignored".into());
                    self.revisions.status += 1;
                }
                let was_current = matches!(
                    self.activity.as_ref().map(|activity| &activity.phase),
                    Some(ActivityPhase::RunningTool(current))
                        if ended_name.as_ref().is_some_and(|ended| current == ended)
                );
                if ended && was_current && self.working && self.terminal_tail.is_none() {
                    self.transition_activity(ActivityPhase::AwaitingProvider);
                }
                if ended {
                    if slim_core::tool_call_is_read_only(&name) {
                        self.tools_used_read += 1;
                    } else {
                        self.tools_used_mutating += 1;
                    }
                    self.maybe_warn_tool_budget();
                }
                self.revisions.content += 1;
            }
            UiEvent::ActivityChanged { label } => {
                if self.terminal_tail.is_none() {
                    self.transition_activity(ActivityPhase::External(label));
                }
            }
            UiEvent::ProviderPhaseChanged {
                phase,
                label,
                elapsed_ms,
            } => {
                match phase {
                    slim_core::ProviderPhase::Connecting => {
                        self.provider_timings = ProviderTimingState::default();
                    }
                    slim_core::ProviderPhase::HeadersReceived => {
                        self.provider_timings.headers_ms = Some(elapsed_ms);
                    }
                    slim_core::ProviderPhase::FirstByte => {
                        self.provider_timings.first_byte_ms = Some(elapsed_ms);
                    }
                    slim_core::ProviderPhase::FirstSemantic => {
                        self.provider_timings.first_semantic_ms = Some(elapsed_ms);
                    }
                    slim_core::ProviderPhase::Compacting => {}
                    slim_core::ProviderPhase::PreparingTool => {}
                }
                // FirstSemantic is a timing fence shared by text, reasoning,
                // usage and tool-call events. The following semantic event owns
                // the visible activity, so do not briefly mislabel tool-only
                // turns as a textual response.
                if self.terminal_tail.is_none() && phase != slim_core::ProviderPhase::FirstSemantic
                {
                    self.transition_activity(ActivityPhase::External(label));
                }
            }
            UiEvent::ApprovalRequired {
                request_id,
                summary,
                persisted,
            } => self.apply_interaction_request(InteractionRequestState {
                request_id,
                kind: InteractionRequestKind::Approval { summary },
                persisted,
                response_pending: false,
                acknowledgement: None,
                selected_question_option: 0,
                custom_question_answer: false,
                answered: None,
            }),
            UiEvent::InputRequired {
                request_id,
                prompt,
                options,
                persisted,
            } => self.apply_interaction_request(InteractionRequestState {
                request_id,
                kind: InteractionRequestKind::Input { prompt, options },
                persisted,
                response_pending: false,
                acknowledgement: None,
                selected_question_option: 0,
                custom_question_answer: false,
                answered: None,
            }),
            UiEvent::QuestionRequired {
                request_id,
                question,
                options,
                persisted,
            } => self.apply_interaction_request(InteractionRequestState {
                request_id,
                kind: InteractionRequestKind::Question { question, options },
                persisted,
                response_pending: false,
                acknowledgement: None,
                selected_question_option: 0,
                custom_question_answer: false,
                answered: None,
            }),
            UiEvent::InteractionAcknowledged {
                request_id,
                accepted,
                message,
            } => self.acknowledge_interaction(&request_id, accepted, message),
            UiEvent::QueuedUserAdded { text, position } => {
                let id = self.fresh_id("queued");
                self.blocks.push(Block::new(
                    id,
                    BlockKind::QueuedUser(format!("queued[{position}] {text}")),
                    BlockLifecycle::Pending,
                ));
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::TodoChanged { items } => {
                self.todo_items = items;
                self.todo_dock_open = self.todo_items.iter().any(|item| {
                    !matches!(
                        item.status,
                        crate::api::TodoItemStatus::Completed
                            | crate::api::TodoItemStatus::Cancelled
                    )
                });
                self.revisions.status += 1;
            }
            UiEvent::ContentPageLoaded {
                handle,
                request_id,
                cursor,
                text,
                next_cursor,
            } => {
                let Some(block) = self.blocks.iter_mut().find(|block| {
                    matches!(block.kind(), BlockKind::Tool(state)
                        if state.content_handle.as_ref() == Some(&handle)
                            && state.pending_page.as_ref().is_some_and(|pending|
                                pending.request_id == request_id && pending.cursor == cursor))
                }) else {
                    return;
                };
                let cursor_start = cursor.map_or(0, |cursor| cursor.0);
                let expected_next = u64::try_from(text.len())
                    .ok()
                    .and_then(|len| cursor_start.checked_add(len));
                let invalid_page = text.len() > 16 * 1024
                    || next_cursor.is_some_and(|next| {
                        next.0 <= cursor_start || Some(next.0) != expected_next
                    });
                if invalid_page {
                    if let Some(state) = block.tool_state_mut() {
                        state.pending_page = None;
                    }
                    self.push_notification("Invalid content page boundary".into());
                    self.revisions.status += 1;
                } else if block.append_tool_page(&text, next_cursor) {
                    self.revisions.content += 1;
                } else {
                    if let Some(state) = block.tool_state_mut() {
                        state.pending_page = None;
                    }
                    self.push_notification("Content page exceeds the 2 MiB block limit".into());
                    self.revisions.status += 1;
                }
            }
            UiEvent::ContentPageFailed {
                handle,
                request_id,
                cursor,
                message,
            } => {
                let Some(block) = self.blocks.iter_mut().find(|block| {
                    matches!(block.kind(), BlockKind::Tool(state)
                        if state.content_handle.as_ref() == Some(&handle)
                            && state.pending_page.as_ref().is_some_and(|pending|
                                pending.request_id == request_id && pending.cursor == cursor))
                }) else {
                    return;
                };
                if let Some(state) = block.tool_state_mut() {
                    state.pending_page = None;
                }
                self.push_notification(message);
                self.revisions.status += 1;
            }
            UiEvent::Notification { message } => {
                if message.starts_with("Connected:") {
                    self.dismiss_notifications_starting_with("No provider connected");
                }
                self.push_notification(message);
                self.revisions.status += 1;
            }
            UiEvent::CompactionCompleted => {
                self.compaction_status = slim_core::context::CompactionStatus::Idle;
                let id = self.fresh_id("compaction");
                let mut block = Block::new(
                    id,
                    BlockKind::System("compaction completed".into()),
                    BlockLifecycle::Complete,
                );
                block.fold = FoldState::Collapsed;
                self.blocks.push(block);
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::CompactionState { state, .. } => {
                self.compaction_status = state;
                self.revisions.status += 1;
            }
            UiEvent::FatalError { run_id, message } => {
                let terminal_run_id = run_id.or(self.active_run_id);
                let accepted = terminal_run_id
                    .is_none_or(|run_id| self.accept_terminal(run_id, BlockLifecycle::Failed));
                if accepted {
                    self.terminalize_streaming(BlockLifecycle::Failed);
                    self.terminalize_latest_assistant(BlockLifecycle::Failed);
                    self.close_request_usage(false);
                    self.working = false;
                    self.run_started_ms = None;
                    self.activity = None;
                    let id = self.fresh_id("error");
                    self.blocks.push(Block::new(
                        id,
                        BlockKind::Error(message),
                        BlockLifecycle::Failed,
                    ));
                    self.note_new_content();
                    self.revisions.content += 1;
                    self.revisions.status += 1;
                }
            }
            UiEvent::Shutdown => self.shutdown = true,
        }
    }
}
