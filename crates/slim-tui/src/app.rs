use slim_core::OperatingMode;

use crate::api::{LoginProvider, ModelAlias, ReasoningEffort, SensitiveText, SessionId, UiEvent};
use crate::block::{Block, BlockKind, BlockLifecycle, FoldState, ToolState};
use crate::composer::Composer;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RevisionSet {
    pub content: u64,
    pub fold: u64,
    pub theme: u64,
    pub viewport: u64,
    pub focus: u64,
    pub status: u64,
}

/// Scroll state per spec §13.3: live edge follows the newest content; pinned
/// keeps an offset counted from the bottom so appends never move the view.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScrollState {
    pub pinned: bool,
    pub offset_from_end: usize,
    pub unseen: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoginOverlay {
    pub selected: usize,
    pub in_progress: bool,
    pub progress: Option<String>,
    pub auth_url: Option<SensitiveText>,
    pub user_code: Option<SensitiveText>,
}

impl LoginOverlay {
    pub fn provider(&self) -> LoginProvider {
        if self.selected == 0 {
            LoginProvider::Anthropic
        } else {
            LoginProvider::OpenAiCodex
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelOverlay {
    pub selected: usize,
}

impl ModelOverlay {
    pub fn alias(&self) -> ModelAlias {
        ModelAlias::from_index(self.selected)
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
    pub auth_provider: Option<LoginProvider>,
    pub authenticated: bool,
    pub login_overlay: Option<LoginOverlay>,
    pub model_overlay: Option<ModelOverlay>,
    pub effort_overlay: Option<EffortOverlay>,
    /// Command palette query while open (None = closed).
    pub palette_query: Option<String>,
    /// Slash autocomplete while the token under edit matches a command
    /// (None = closed). Triggered by `/` anywhere in the draft.
    pub slash_suggestions: Option<SlashSuggestions>,
    pub blocks: Vec<Block>,
    pub notifications: Vec<String>,
    pub composer: Composer,
    pub todo_items: Vec<crate::api::TodoItemView>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub working: bool,
    pub todo_dock_open: bool,
    pub scroll: ScrollState,
    pub spinner_frame: u64,
    pub next_block_id: u64,
    pub shutdown: bool,
    pub revisions: RevisionSet,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            session_id: None,
            cwd: String::new(),
            mode: OperatingMode::Auto,
            model: ModelAlias::Sol.id().into(),
            effort: ReasoningEffort::High,
            auth_provider: None,
            authenticated: false,
            login_overlay: None,
            model_overlay: None,
            effort_overlay: None,
            palette_query: None,
            slash_suggestions: None,
            blocks: Vec::new(),
            notifications: Vec::new(),
            composer: Composer::default(),
            todo_items: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            working: false,
            todo_dock_open: false,
            scroll: ScrollState::default(),
            spinner_frame: 0,
            next_block_id: 0,
            shutdown: false,
            revisions: RevisionSet::default(),
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_snapshot(&mut self, session_id: SessionId, cwd: String) {
        self.session_id = Some(session_id);
        self.cwd = cwd;
        self.revisions.status += 1;
    }

    pub fn push_notification(&mut self, message: String) {
        if self.notifications.len() == 100 {
            self.notifications.remove(0);
        }
        self.notifications.push(message);
    }

    /// Stable monotonic block ids (spec §2 "blocos tipados com IDs e revisions
    /// estáveis"); index-based ids collided after removals.
    fn fresh_id(&mut self, kind: &str) -> String {
        let id = format!("{kind}-{}", self.next_block_id);
        self.next_block_id += 1;
        id
    }

    /// New content while the user is pinned away from live edge increments the
    /// unseen counter (spec §13.3).
    fn note_new_content(&mut self) {
        if self.scroll.pinned {
            self.scroll.unseen = self.scroll.unseen.saturating_add(1);
        }
    }

    pub fn apply_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::SessionSnapshot { session_id, cwd } => self.apply_snapshot(session_id, cwd),
            UiEvent::RunStarted => {
                self.working = true;
                self.revisions.status += 1;
            }
            UiEvent::RunCompleted => {
                self.working = false;
                self.revisions.status += 1;
            }
            UiEvent::RunStopped { message } => {
                self.working = false;
                self.push_notification(message);
                self.revisions.status += 1;
            }
            UiEvent::RunCancelled => {
                self.working = false;
                self.push_notification("run cancelled".into());
                self.revisions.status += 1;
            }
            UiEvent::RunFailed { message } => {
                self.working = false;
                let id = self.fresh_id("error");
                self.blocks.push(Block::new(
                    id,
                    BlockKind::Error(message),
                    BlockLifecycle::Failed,
                ));
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::UserMessageAdded { text } => {
                let id = self.fresh_id("user");
                self.blocks.push(Block::new(
                    id,
                    BlockKind::User(text),
                    BlockLifecycle::Complete,
                ));
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::RestoreDraft { text } => {
                if self.composer.payload().is_empty() {
                    self.composer.insert_text(text);
                    self.revisions.content += 1;
                }
            }
            UiEvent::AssistantDelta { text } => {
                // RunStarted is the authority for working state; deltas no
                // longer mask lost lifecycle events (tracker L3/B6).
                if let Some(Block {
                    kind: BlockKind::Assistant(current),
                    lifecycle,
                    ..
                }) = self.blocks.last_mut()
                {
                    current.push_str(&text);
                    *lifecycle = BlockLifecycle::Streaming;
                } else {
                    let id = self.fresh_id("assistant");
                    self.blocks.push(Block::new(
                        id,
                        BlockKind::Assistant(text),
                        BlockLifecycle::Streaming,
                    ));
                }
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::AssistantEnded => {
                if let Some(block) = self.blocks.last_mut() {
                    if matches!(block.kind, BlockKind::Assistant(_)) {
                        block.lifecycle = BlockLifecycle::Complete;
                    }
                }
                self.revisions.content += 1;
            }
            UiEvent::ThinkingDelta { text } => {
                if let Some(Block {
                    kind: BlockKind::Thinking(current),
                    ..
                }) = self.blocks.last_mut()
                {
                    current.push_str(&text);
                } else {
                    let mut block = Block::new(
                        self.fresh_id("thinking"),
                        BlockKind::Thinking(text),
                        BlockLifecycle::Streaming,
                    );
                    block.fold = FoldState::Collapsed;
                    self.blocks.push(block);
                }
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                self.input_tokens = self.input_tokens.saturating_add(input_tokens as u64);
                self.output_tokens = self.output_tokens.saturating_add(output_tokens as u64);
                self.revisions.status += 1;
            }
            UiEvent::ModeChanged { mode } => {
                self.mode = mode;
                self.revisions.status += 1;
            }
            UiEvent::ModelChanged { model } => {
                self.model = model;
                self.model_overlay = None;
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
            UiEvent::LoginUrl { url, user_code } => {
                if let Some(overlay) = self.login_overlay.as_mut() {
                    overlay.in_progress = true;
                    overlay.auth_url = Some(url);
                    overlay.user_code = user_code;
                }
                self.revisions.status += 1;
            }
            UiEvent::ToolStarted { name } => {
                let id = self.fresh_id("tool");
                self.blocks.push(Block::new(
                    id,
                    BlockKind::Tool(ToolState {
                        name,
                        preview: String::new(),
                    }),
                    BlockLifecycle::Streaming,
                ));
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::ToolProgress { name, preview } => {
                if let Some(block) = self.blocks.iter_mut().rev().find(|block| {
                    matches!(&block.kind, BlockKind::Tool(state) if state.name == name)
                        && block.lifecycle == BlockLifecycle::Streaming
                }) {
                    if let BlockKind::Tool(state) = &mut block.kind {
                        state.preview = preview;
                    }
                }
                self.revisions.content += 1;
            }
            UiEvent::ToolEnded { name, success } => {
                if let Some(block) = self.blocks.iter_mut().rev().find(|block| {
                    matches!(&block.kind, BlockKind::Tool(state) if state.name == name)
                        && block.lifecycle == BlockLifecycle::Streaming
                }) {
                    block.lifecycle = if success {
                        BlockLifecycle::Complete
                    } else {
                        BlockLifecycle::Failed
                    };
                }
                self.revisions.content += 1;
            }
            UiEvent::ActivityChanged { label } => {
                let id = self.fresh_id("activity");
                self.blocks.push(Block::new(
                    id,
                    BlockKind::Activity(label),
                    BlockLifecycle::Streaming,
                ));
                self.note_new_content();
                self.revisions.content += 1;
            }
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
                self.todo_dock_open = !self.todo_items.is_empty();
                self.revisions.status += 1;
            }
            UiEvent::ContentPageLoaded { handle, text } => {
                self.push_notification(format!(
                    "content {} loaded ({} chars)",
                    handle.0,
                    text.chars().count()
                ));
                self.revisions.status += 1;
            }
            UiEvent::Notification { message } => {
                self.push_notification(message);
                self.revisions.status += 1;
            }
            UiEvent::FatalError { message } => {
                let id = self.fresh_id("error");
                self.blocks.push(Block::new(
                    id,
                    BlockKind::Error(message),
                    BlockLifecycle::Failed,
                ));
                self.note_new_content();
                self.revisions.content += 1;
            }
            UiEvent::Shutdown => self.shutdown = true,
        }
    }
}
