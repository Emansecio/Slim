//! Three-stage speculative execution during SSE streaming:
//! - Stage 1 (streaming deltas): non-blocking slab + LSP `didOpen` pre-warm
//!   and QuickJS worker pre-warm before tool arguments finish streaming.
//! - Stage 2 (call completion across all wires): speculatively dispatch
//!   read-only `read`/`list`/`search`, `code_intel`, and allowlisted readonly
//!   `git` shell calls on `Responses`, `Anthropic`, and `Chat`/`Devin` wires.
//! - Stage 3 (`SpeculativeHazardTracker`): fail-closed tracking of mutated
//!   paths so disjoint `read`/`list` calls after clean `write`/`patch` still
//!   overlap while overlapping/ambiguous reads barrier cleanly.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Semaphore;

use super::tool_schedule::is_serial_barrier;
use super::*;
use crate::provider::ProviderEvent;

const MAX_PREWARM_PATHS_PER_TURN: usize = 16;
const MAX_PREWARM_CONCURRENCY: usize = 4;

pub(crate) fn speculative_v2_enabled() -> bool {
    !std::env::var_os("SLIM_SPECULATIVE_V2")
        .is_some_and(|v| v == "0" || v.eq_ignore_ascii_case("false"))
}

pub(crate) enum SpeculativeOutcome {
    Native {
        fingerprint: String,
        outcome: ToolExecutionOutcome,
    },
    CodeIntel {
        fingerprint: String,
        result: ToolResult,
        presentation: Option<crate::tools::CodeIntelPresentation>,
    },
    ReadOnlyShell {
        fingerprint: String,
        outcome: ToolExecutionOutcome,
    },
    CodeMode(super::codemode::SpeculativeCodeModeOutcome),
}

pub(crate) struct SpeculativeReadEntry {
    pub(crate) fingerprint: String,
    pub(crate) revision_before: u64,
    pub(crate) fs_epoch_before: u64,
    pub(crate) git_dirty_epoch_before: u64,
    pub(crate) handle: tokio::task::JoinHandle<Option<SpeculativeOutcome>>,
}

#[derive(Default)]
struct JsonObjectTracker {
    started: bool,
    depth: usize,
    in_string: bool,
    escaped: bool,
    closed: bool,
}

impl JsonObjectTracker {
    fn push(&mut self, fragment: &str) -> bool {
        if self.closed {
            return true;
        }
        for ch in fragment.chars() {
            if !self.started {
                if ch.is_whitespace() {
                    continue;
                }
                if ch == '{' {
                    self.started = true;
                    self.depth = 1;
                } else {
                    // Not an object literal; stop trying brace completion.
                    return false;
                }
                continue;
            }
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if ch == '\\' {
                    self.escaped = true;
                } else if ch == '"' {
                    self.in_string = false;
                }
                continue;
            }
            match ch {
                '"' => self.in_string = true,
                '{' => self.depth = self.depth.saturating_add(1),
                '}' => {
                    self.depth = self.depth.saturating_sub(1);
                    if self.depth == 0 {
                        self.closed = true;
                        return true;
                    }
                }
                _ => {}
            }
        }
        self.closed
    }
}

struct StreamingCallBuffer {
    id: String,
    name: String,
    args: String,
    scanned_offset: usize,
    in_codemode_list: bool,
    wants_index: bool,
    dispatched: bool,
    tracker: JsonObjectTracker,
}

impl StreamingCallBuffer {
    fn new(id: String, name: String) -> Self {
        Self {
            id,
            name,
            args: String::new(),
            scanned_offset: 0,
            in_codemode_list: false,
            wants_index: false,
            dispatched: false,
            tracker: JsonObjectTracker::default(),
        }
    }
}

#[derive(Default)]
pub(super) struct SpeculativeHazardTracker {
    hard_barrier: bool,
    open_non_readonly: HashSet<String>,
    mutated_paths: Vec<PathBuf>,
    mutated_keys: HashSet<String>,
}

impl SpeculativeHazardTracker {
    pub(super) fn on_call_started(&mut self, key: &str, name: &str) {
        if !matches!(name, "read" | "list" | "search" | "code_intel") {
            self.open_non_readonly.insert(key.to_owned());
        }
    }

    pub(super) fn record_non_readonly_complete(
        &mut self,
        key: &str,
        name: &str,
        prepared: Option<&PreparedToolInvocation>,
    ) {
        self.open_non_readonly.remove(key);
        if self.hard_barrier {
            return;
        }
        if let Some(prep) = prepared {
            if matches!(name, "write" | "patch")
                && prep.error.is_none()
                && !is_serial_barrier(prep)
                && !prep.target_paths.is_empty()
            {
                for path in &prep.target_paths {
                    self.mutated_paths.push(path.clone());
                    self.mutated_keys
                        .insert(crate::tools::path_identity(path));
                }
                return;
            }
        }
        self.hard_barrier = true;
    }

    pub(super) fn force_barrier(&mut self) {
        self.hard_barrier = true;
    }

    pub(super) fn can_speculate(&self, name: &str, prepared: &PreparedToolInvocation) -> bool {
        if self.hard_barrier || !self.open_non_readonly.is_empty() {
            return false;
        }
        if self.mutated_paths.is_empty() {
            return true;
        }
        // Post-mutation Stage 3: only disjoint single-path `read` or non-ancestor `list`
        // may speculate ahead.
        match name {
            "read" => {
                prepared.target_paths.len() == 1
                    && !self
                        .mutated_keys
                        .contains(&crate::tools::path_identity(&prepared.target_paths[0]))
            }
            "list" => {
                if prepared.target_paths.len() != 1 {
                    return false;
                }
                let list_root = &prepared.target_paths[0];
                !self
                    .mutated_paths
                    .iter()
                    .any(|mutated| mutated.starts_with(list_root) || list_root.starts_with(mutated))
            }
            _ => false,
        }
    }
}

pub(crate) struct StreamSpeculatorConfig {
    pub(crate) mode: crate::OperatingMode,
    pub(crate) cwd: PathBuf,
    pub(crate) canonical_workspace: Option<PathBuf>,
    pub(crate) max_read_calls: usize,
    pub(crate) tools: ToolRegistry,
    pub(crate) code_intel: Option<Arc<dyn CodeIntelligence>>,
    pub(crate) spec_cancel: CancellationToken,
    pub(crate) run_cancel: Option<CancellationToken>,
    pub(crate) sensitive_values: Vec<String>,
}

pub(crate) struct StreamSpeculator {
    cfg: StreamSpeculatorConfig,
    prewarm_sem: Arc<Semaphore>,
    warmed_paths: HashSet<String>,
    prewarmed_codemode: bool,
    prewarmed_index: bool,
    anthropic_blocks: HashMap<u32, StreamingCallBuffer>,
    chat_slots: HashMap<u32, StreamingCallBuffer>,
    active_chat_index: Option<u32>,
    hazards: SpeculativeHazardTracker,
    dispatched: Vec<(String, SpeculativeReadEntry)>,
    prewarm_handles: Vec<tokio::task::JoinHandle<()>>,
    aborted_by_secret: bool,
}

impl StreamSpeculator {
    pub(super) fn new(cfg: StreamSpeculatorConfig) -> Self {
        Self {
            cfg,
            prewarm_sem: Arc::new(Semaphore::new(MAX_PREWARM_CONCURRENCY)),
            warmed_paths: HashSet::new(),
            prewarmed_codemode: false,
            prewarmed_index: false,
            anthropic_blocks: HashMap::new(),
            chat_slots: HashMap::new(),
            active_chat_index: None,
            hazards: SpeculativeHazardTracker::default(),
            dispatched: Vec::new(),
            prewarm_handles: Vec::new(),
            aborted_by_secret: false,
        }
    }

    pub(super) fn on_event(&mut self, event: &ProviderEvent) {
        if self.aborted_by_secret || self.cfg.run_cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return;
        }
        match event {
            // Wire::Responses complete tool call
            ProviderEvent::ToolCallComplete {
                id,
                name,
                arguments,
                ..
            } => {
                self.dispatch_complete_call(id, name, arguments);
            }
            // Wire::Anthropic streaming tool call
            ProviderEvent::ToolCallStart { index, id, name } => {
                self.hazards.on_call_started(&format!("a:{index}"), name);
                if matches!(name.as_str(), "codemode" | "batch_code") {
                    self.trigger_codemode_prewarm();
                }
                self.anthropic_blocks.insert(
                    *index,
                    StreamingCallBuffer::new(id.clone(), name.clone()),
                );
            }
            ProviderEvent::ToolCallInputDelta { index, partial_json } => {
                let Some(buf) = self.anthropic_blocks.get_mut(index) else {
                    return;
                };
                buf.args.push_str(partial_json);
                self.scan_buffer_for_prewarm_anthropic(*index);
            }
            ProviderEvent::ContentBlockStop { index } => {
                if let Some(mut buf) = self.anthropic_blocks.remove(index) {
                    if !buf.dispatched && !buf.id.is_empty() && !buf.name.is_empty() {
                        buf.dispatched = true;
                        let args = if buf.args.trim().is_empty() {
                            "{}"
                        } else {
                            buf.args.as_str()
                        };
                        self.hazards.open_non_readonly.remove(&format!("a:{index}"));
                        self.dispatch_complete_call(&buf.id, &buf.name, args);
                    }
                }
            }
            // Wire::Chat / Devin streaming tool call deltas
            ProviderEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                let effective_index = index.unwrap_or_else(|| self.active_chat_index.unwrap_or(0));
                if self
                    .active_chat_index
                    .is_some_and(|prev| prev != effective_index)
                {
                    if let Some(prev_idx) = self.active_chat_index {
                        self.flush_chat_slot(prev_idx);
                    }
                }
                self.active_chat_index = Some(effective_index);
                let mut need_codemode_prewarm = false;
                let closed = {
                    let slot = self
                        .chat_slots
                        .entry(effective_index)
                        .or_insert_with(|| StreamingCallBuffer::new(String::new(), String::new()));
                    if let Some(id_val) = id.as_deref().filter(|s| !s.is_empty()) {
                        slot.id = id_val.to_owned();
                    }
                    if let Some(name_val) = name.as_deref().filter(|s| !s.is_empty()) {
                        slot.name = name_val.to_owned();
                        self.hazards
                            .on_call_started(&format!("c:{effective_index}"), name_val);
                        if matches!(name_val, "codemode" | "batch_code") {
                            need_codemode_prewarm = true;
                        }
                    }
                    if !arguments.is_empty() {
                        slot.args.push_str(arguments);
                        slot.tracker.push(arguments)
                    } else {
                        false
                    }
                };
                if need_codemode_prewarm {
                    self.trigger_codemode_prewarm();
                }
                if !arguments.is_empty() {
                    self.scan_buffer_for_prewarm_chat(effective_index);
                    if closed {
                        self.flush_chat_slot(effective_index);
                    }
                }
            }
            ProviderEvent::ToolCall { name, arguments } => {
                if contains_sensitive(&self.cfg.sensitive_values, name)
                    || super::stream_normalizer::sensitive_tool_arguments(
                        arguments,
                        &self.cfg.sensitive_values,
                    )
                {
                    self.aborted_by_secret = true;
                    self.cfg.spec_cancel.cancel();
                } else {
                    self.hazards.force_barrier();
                }
            }
            ProviderEvent::Stopped { .. } => {
                let remaining_chat: Vec<u32> = self.chat_slots.keys().copied().collect();
                for idx in remaining_chat {
                    self.flush_chat_slot(idx);
                }
            }
            _ => {}
        }
    }

    pub(super) fn finish(self, stream_ok: bool) -> (Vec<(String, SpeculativeReadEntry)>, Vec<tokio::task::JoinHandle<()>>) {
        if !stream_ok || self.aborted_by_secret {
            self.cfg.spec_cancel.cancel();
            for (_, entry) in self.dispatched {
                entry.handle.abort();
            }
            for h in self.prewarm_handles {
                h.abort();
            }
            return (Vec::new(), Vec::new());
        }
        (self.dispatched, self.prewarm_handles)
    }

    fn flush_chat_slot(&mut self, index: u32) {
        let Some(slot) = self.chat_slots.get_mut(&index) else {
            return;
        };
        if slot.dispatched || slot.id.is_empty() || slot.name.is_empty() {
            return;
        }
        let candidate = if slot.args.trim().is_empty() {
            "{}".to_owned()
        } else {
            slot.args.clone()
        };
        if matches!(
            super::tool_arguments::object_arguments(&candidate).1,
            super::tool_arguments::ArgumentShape::Object
        ) {
            slot.dispatched = true;
            let id = slot.id.clone();
            let name = slot.name.clone();
            self.hazards.open_non_readonly.remove(&format!("c:{index}"));
            self.dispatch_complete_call(&id, &name, &candidate);
        }
    }

    fn scan_buffer_for_prewarm_anthropic(&mut self, index: u32) {
        let (allow_lsp, wants_index, paths) = {
            let Some(buf) = self.anthropic_blocks.get_mut(&index) else {
                return;
            };
            let (allow_lsp, paths) = extract_paths_from_buffer(buf);
            (allow_lsp, buf.wants_index, paths)
        };
        if wants_index {
            self.trigger_index_prewarm();
        }
        self.enqueue_path_prewarms(allow_lsp, &paths);
    }

    fn scan_buffer_for_prewarm_chat(&mut self, index: u32) {
        let (allow_lsp, wants_index, paths) = {
            let Some(buf) = self.chat_slots.get_mut(&index) else {
                return;
            };
            let (allow_lsp, paths) = extract_paths_from_buffer(buf);
            (allow_lsp, buf.wants_index, paths)
        };
        if wants_index {
            self.trigger_index_prewarm();
        }
        self.enqueue_path_prewarms(allow_lsp, &paths);
    }

    fn trigger_codemode_prewarm(&mut self) {
        if !self.prewarmed_codemode {
            self.prewarmed_codemode = true;
            super::codemode::prewarm_worker_cell();
        }
    }

    fn trigger_index_prewarm(&mut self) {
        if !self.prewarmed_index {
            self.prewarmed_index = true;
            if let Some(root) = self.cfg.canonical_workspace.clone() {
                let handle = tokio::spawn(async move {
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = crate::project_index::for_workspace(&root);
                    })
                    .await;
                });
                self.prewarm_handles.push(handle);
            }
        }
    }

    fn enqueue_path_prewarms(&mut self, allow_lsp_spawn: bool, raw_paths: &[String]) {
        if raw_paths.is_empty() {
            return;
        }
        let Some(canon_ws) = self.cfg.canonical_workspace.as_ref() else {
            return;
        };
        for raw in raw_paths {
            if self.warmed_paths.len() >= MAX_PREWARM_PATHS_PER_TURN {
                break;
            }
            if contains_sensitive(&self.cfg.sensitive_values, raw) {
                self.aborted_by_secret = true;
                self.cfg.spec_cancel.cancel();
                return;
            }
            if !self.warmed_paths.insert(raw.clone()) {
                continue;
            }
            let Ok(abs) = crate::tools::resolve_workspace_path_from_root(canon_ws, raw) else {
                continue;
            };
            let sem = Arc::clone(&self.prewarm_sem);
            let ws_root = self.cfg.cwd.clone();
            let code_intel = self.cfg.code_intel.clone();
            let cancel = self.cfg.spec_cancel.clone();
            let handle = tokio::spawn(async move {
                let Ok(_permit) = sem.acquire_owned().await else {
                    return;
                };
                if cancel.is_cancelled() {
                    return;
                }
                if let Some(daemon) = crate::project_index::existing_for_path(&abs) {
                    let abs_slab = abs.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = daemon.read_file_cached(&abs_slab);
                    })
                    .await;
                }
                if cancel.is_cancelled() {
                    return;
                }
                if let Some(ci) = code_intel {
                    ci.prewarm_file(&ws_root, &abs, allow_lsp_spawn, Some(cancel))
                        .await;
                }
            });
            self.prewarm_handles.push(handle);
        }
    }

    fn dispatch_complete_call(&mut self, id: &str, name: &str, arguments: &str) {
        if id.is_empty() || name.is_empty() {
            return;
        }
        if contains_sensitive(&self.cfg.sensitive_values, name)
            || super::stream_normalizer::sensitive_tool_arguments(
                arguments,
                &self.cfg.sensitive_values,
            )
        {
            self.aborted_by_secret = true;
            self.cfg.spec_cancel.cancel();
            return;
        }
        if super::mode::code_mode_direct_refusal(self.cfg.mode, name).is_some() {
            self.hazards.force_barrier();
            return;
        }
        if matches!(name, "codemode" | "batch_code") {
            self.trigger_codemode_prewarm();
            let mut cm_paths = Vec::new();
            let wants_lsp = collect_codemode_read_literals(arguments, &mut cm_paths);
            if !cm_paths.is_empty() {
                self.enqueue_path_prewarms(wants_lsp, &cm_paths);
            }
            if arguments.contains("slim.grepIndex(") || arguments.contains("slim.graph(") {
                self.trigger_index_prewarm();
            }
            let can_speculate_ro = !self.hazards.hard_barrier
                && self.hazards.open_non_readonly.is_empty()
                && self.hazards.mutated_paths.is_empty()
                && self.dispatched.len() < self.cfg.max_read_calls
                && (name == "batch_code" || self.cfg.mode == crate::OperatingMode::Code);
            if can_speculate_ro {
                if let Ok(val) = serde_json::from_str::<Value>(arguments) {
                    if let Some(code) = val.get("code").and_then(Value::as_str) {
                        if super::codemode::is_strictly_readonly_program(code) {
                            let revision_before = self.cfg.tools.workspace_revision();
                            let (fs_epoch_before, git_dirty_epoch_before) = self
                                .cfg
                                .canonical_workspace
                                .as_ref()
                                .and_then(|root| crate::project_index::existing_for_workspace(root))
                                .map_or((0, 0), |d| (d.fs_epoch(), d.git_dirty_epoch()));
                            let fingerprint = crate::tools::hash_fields(&[
                                b"slim-spec-codemode-v1",
                                name.as_bytes(),
                                arguments.as_bytes(),
                            ]);
                            let mode = self.cfg.mode;
                            let cwd = self.cfg.cwd.clone();
                            let call_name = name.to_owned();
                            let args_owned = arguments.to_owned();
                            let max_read_calls = self.cfg.max_read_calls;
                            let tools = self.cfg.tools.clone();
                            let code_intel = self.cfg.code_intel.clone();
                            let spec_cancel = self.cfg.spec_cancel.clone();
                            let secrets = self.cfg.sensitive_values.clone();
                            let handle = tokio::spawn(async move {
                                super::codemode::run_readonly_speculative_cell(
                                    mode,
                                    cwd,
                                    call_name,
                                    args_owned,
                                    max_read_calls,
                                    tools,
                                    code_intel,
                                    spec_cancel,
                                    secrets,
                                )
                                .await
                                .map(SpeculativeOutcome::CodeMode)
                            });
                            self.dispatched.push((
                                id.to_owned(),
                                SpeculativeReadEntry {
                                    fingerprint,
                                    revision_before,
                                    fs_epoch_before,
                                    git_dirty_epoch_before,
                                    handle,
                                },
                            ));
                            return;
                        }
                    }
                }
            }
            self.hazards.force_barrier();
            return;
        }

        let is_shell_ro = name == "shell"
            && self.cfg.mode.allows_mutation()
            && is_allowlisted_readonly_git(arguments);
        let is_native_ro = matches!(name, "read" | "list" | "search" | "code_intel");
        if !is_native_ro && !is_shell_ro {
            if matches!(name, "write" | "patch") {
                let prepared = self.cfg.tools.prepare_invocation(
                    self.cfg.mode,
                    &self.cfg.cwd,
                    name,
                    arguments,
                );
                self.hazards
                    .record_non_readonly_complete(id, name, Some(&prepared));
            } else {
                self.hazards.record_non_readonly_complete(id, name, None);
            }
            return;
        }

        if self.dispatched.len() >= self.cfg.max_read_calls {
            return;
        }

        let prepared = self.cfg.tools.prepare_invocation(
            self.cfg.mode,
            &self.cfg.cwd,
            name,
            arguments,
        );
        if prepared.error.is_some() || !self.hazards.can_speculate(name, &prepared) {
            return;
        }

        let revision_before = self.cfg.tools.workspace_revision();
        let (fs_epoch_before, git_dirty_epoch_before) = self
            .cfg
            .canonical_workspace
            .as_ref()
            .and_then(|root| crate::project_index::existing_for_workspace(root))
            .map_or((0, 0), |d| (d.fs_epoch(), d.git_dirty_epoch()));
        let fingerprint = prepared.canonical_fingerprint.clone();

        if name == "code_intel" {
            let code_intel = self.cfg.code_intel.clone();
            let cancel = self.cfg.spec_cancel.clone();
            let req = super::native_code_intel::prepared_code_intel_request(&prepared);
            let fp = fingerprint.clone();
            let handle = tokio::spawn(async move {
                if cancel.is_cancelled() {
                    return None;
                }
                let (result, presentation) =
                    super::native_code_intel::run_code_intel_request(
                        code_intel,
                        req,
                        Some(cancel),
                    )
                    .await;
                Some(SpeculativeOutcome::CodeIntel {
                    fingerprint: fp,
                    result,
                    presentation,
                })
            });
            self.dispatched.push((
                id.to_owned(),
                SpeculativeReadEntry {
                    fingerprint,
                    revision_before,
                    fs_epoch_before,
                    git_dirty_epoch_before,
                    handle,
                },
            ));
            return;
        }

        let tools_clone = self.cfg.tools.clone();
        let spec_cancel = self.cfg.spec_cancel.clone();
        let run_cancel = self.cfg.run_cancel.clone();
        let fp = fingerprint.clone();
        let is_shell = is_shell_ro;
        let handle = tokio::task::spawn_blocking(move || {
            if run_cancel
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
                || spec_cancel.is_cancelled()
            {
                return None;
            }
            let _native_work = run_cancel
                .as_ref()
                .map(CancellationToken::track_native_work);
            if !is_shell
                && !prepared
                    .spec
                    .is_some_and(|spec| spec.effect_class == ToolEffectClass::SnapshotRead)
            {
                return None;
            }
            let outcome = tools_clone.execute_prepared_with_cancellation_and_progress(
                &prepared,
                Some(&spec_cancel),
                |_| {},
            );
            Some(if is_shell {
                SpeculativeOutcome::ReadOnlyShell {
                    fingerprint: fp,
                    outcome,
                }
            } else {
                SpeculativeOutcome::Native {
                    fingerprint: fp,
                    outcome,
                }
            })
        });
        self.dispatched.push((
            id.to_owned(),
            SpeculativeReadEntry {
                fingerprint,
                revision_before,
                fs_epoch_before,
                git_dirty_epoch_before,
                handle,
            },
        ));
    }
}

fn contains_sensitive(secrets: &[String], text: &str) -> bool {
    !secrets.is_empty() && super::redaction::redact_values(secrets, text) != text
}

fn extract_paths_from_buffer(buf: &mut StreamingCallBuffer) -> (bool, Vec<String>) {
    let mut paths = Vec::new();
    let is_codemode = matches!(buf.name.as_str(), "codemode" | "batch_code");
    let bytes = buf.args.as_bytes();
    let mut cursor = buf.scanned_offset.min(bytes.len());

    if is_codemode {
        let slice = &buf.args[cursor..];
        if slice.contains("slim.readMany(") || slice.contains("slim.outlineMany(") {
            buf.in_codemode_list = true;
        }
        collect_codemode_read_literals(slice, &mut paths);
        if buf.in_codemode_list {
            collect_quoted_path_literals(slice, &mut paths);
            if slice.contains(']') || slice.contains(')') {
                buf.in_codemode_list = false;
            }
        }
        let wants_lsp = buf.args.contains("slim.code_intel(");
        if slice.contains("slim.grepIndex(") || slice.contains("slim.graph(") {
            buf.wants_index = true;
        }
        buf.scanned_offset = buf
            .args
            .floor_char_boundary(buf.args.len().saturating_sub(64));
        return (wants_lsp, paths);
    }

    while let Some(rel_pos) = buf.args[cursor..].find("\"path\"") {
        let key_pos = cursor + rel_pos;
        let mut p = key_pos + "\"path\"".len();
        while p < bytes.len() && bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        if bytes.get(p) != Some(&b':') {
            cursor = p;
            continue;
        }
        p += 1;
        while p < bytes.len() && bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        if bytes.get(p) != Some(&b'"') {
            cursor = key_pos;
            break;
        }
        let open_quote = p;
        p += 1;
        let mut closed_quote = None;
        while p < bytes.len() {
            if bytes[p] == b'\\' {
                p = p.saturating_add(2);
                continue;
            }
            if bytes[p] == b'"' {
                closed_quote = Some(p);
                break;
            }
            p += 1;
        }
        let Some(end_quote) = closed_quote else {
            // Incomplete JSON string: retry cleanly on next delta.
            cursor = key_pos;
            break;
        };
        if let Ok(decoded) = serde_json::from_str::<String>(&buf.args[open_quote..=end_quote]) {
            if !decoded.trim().is_empty() {
                paths.push(decoded);
            }
        }
        cursor = end_quote + 1;
    }

    if cursor == buf.scanned_offset && !buf.args[cursor..].contains("\"path\"") {
        buf.scanned_offset = buf
            .args
            .floor_char_boundary(buf.args.len().saturating_sub(7));
    } else {
        buf.scanned_offset = cursor;
    }
    (buf.name == "code_intel", paths)
}

fn collect_quoted_path_literals(window: &str, out: &mut Vec<String>) {
    let mut quote_scan = 0;
    let wb = window.as_bytes();
    while quote_scan < wb.len() && out.len() < MAX_PREWARM_PATHS_PER_TURN {
        if wb[quote_scan] == b'\\' && wb.get(quote_scan + 1) == Some(&b'"') {
            let start = quote_scan + 2;
            let mut end = start;
            while end < wb.len() && wb[end] != b'\\' && wb[end] != b'"' && wb[end] != b'\n' {
                end += 1;
            }
            if wb.get(end) == Some(&b'\\') && wb.get(end + 1) == Some(&b'"') {
                let cand = &window[start..end];
                if (cand.contains('/') || cand.contains('.')) && !cand.contains(' ') {
                    out.push(cand.to_owned());
                }
                quote_scan = end + 2;
                continue;
            }
        } else if wb[quote_scan] == b'\'' || wb[quote_scan] == b'"' {
            let q = wb[quote_scan];
            let start = quote_scan + 1;
            let mut end = start;
            while end < wb.len() && wb[end] != q && wb[end] != b'\\' && wb[end] != b'\n' {
                end += 1;
            }
            if end < wb.len() && wb[end] == q {
                let cand = &window[start..end];
                if (cand.contains('/') || cand.contains('.')) && !cand.contains(' ') {
                    out.push(cand.to_owned());
                }
                quote_scan = end + 1;
                continue;
            }
        }
        quote_scan += 1;
    }
}

fn collect_codemode_read_literals(snippet: &str, out: &mut Vec<String>) -> bool {
    let wants_lsp = snippet.contains("slim.code_intel(");
    for needle in [
        "slim.read(",
        "slim.readMany(",
        "slim.outlineMany(",
        "slim.patch(",
        "slim.patchMany(",
        "slim.code_intel(",
    ] {
        let mut idx = 0;
        while let Some(pos) = snippet[idx..].find(needle) {
            let after = &snippet[idx + pos + needle.len()..];
            let cap = if matches!(needle, "slim.readMany(" | "slim.outlineMany(") {
                after.find(')').map_or(after.len().min(2048), |p| p.min(2048))
            } else {
                256
            };
            let window = &after[..after.floor_char_boundary(cap)];
            collect_quoted_path_literals(window, out);
            idx += pos + needle.len();
        }
    }
    wants_lsp
}

pub(super) fn is_allowlisted_readonly_git(args_json: &str) -> bool {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return false;
    };
    is_allowlisted_readonly_shell_inner(&val, true)
}

/// Strictly fail-closed classifier for read-only shell commands (git inspection
/// + simple non-mutating inspection tools with zero shell metacharacters).
pub(super) fn is_allowlisted_readonly_shell_value(val: &Value) -> bool {
    is_allowlisted_readonly_shell_inner(val, false)
}

fn is_allowlisted_readonly_shell_inner(val: &Value, git_only: bool) -> bool {
    let Some(obj) = val.as_object() else {
        return false;
    };
    if obj.get("background").and_then(Value::as_bool) == Some(true)
        || obj.contains_key("yield_ms")
        || obj.contains_key("stdin")
    {
        return false;
    }
    let Some(command) = obj.get("command").and_then(Value::as_str) else {
        return false;
    };
    let trimmed = command.trim();
    let has_shell_meta = |s: &str| {
        s.chars().any(|c| {
            matches!(
                c,
                '&' | '|'
                    | ';'
                    | '>'
                    | '<'
                    | '`'
                    | '$'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '\''
                    | '"'
                    | '\\'
                    | '\n'
                    | '\r'
                    | '*'
                    | '?'
            )
        })
    };
    let (program, tokens): (&str, Vec<&str>) = if let Some(args) =
        obj.get("args").and_then(Value::as_array)
    {
        if trimmed.contains(char::is_whitespace) {
            return false;
        }
        let mut collected = Vec::with_capacity(args.len());
        for a in args {
            let Some(s) = a.as_str() else {
                return false;
            };
            collected.push(s);
        }
        (trimmed, collected)
    } else if obj.get("args").is_some_and(|a| !a.is_null()) || has_shell_meta(trimmed) {
        return false;
    } else {
        let mut parts = trimmed.split_whitespace();
        let Some(prog) = parts.next() else {
            return false;
        };
        (prog, parts.collect())
    };

    if program == "git" {
        let Some(&subcmd) = tokens.first() else {
            return false;
        };
        if !matches!(
            subcmd,
            "status" | "diff" | "log" | "show" | "branch" | "rev-parse"
        ) {
            return false;
        }
        for &arg in &tokens[1..] {
            if arg == "-c"
                || arg == "-C"
                || arg == "-o"
                || arg.starts_with("--git-dir")
                || arg.starts_with("--work-tree")
                || arg.starts_with("--exec-path")
                || arg.starts_with("--output")
                || arg.starts_with("-c=")
            {
                return false;
            }
        }
        if subcmd == "branch" {
            for &arg in &tokens[1..] {
                if !matches!(
                    arg,
                    "--list"
                        | "--show-current"
                        | "-a"
                        | "--all"
                        | "-r"
                        | "--remotes"
                        | "-v"
                        | "-vv"
                ) {
                    return false;
                }
            }
        }
        return true;
    }

    if git_only {
        return false;
    }
    match program {
        "pwd" | "which" | "wc" | "ls" | "head" | "tail" | "stat" | "cat" => true,
        "rg" => !tokens
            .iter()
            .any(|a| *a == "--pre" || a.starts_with("--pre=")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderEvent;
    use crate::runtime::temp_root::TempRoot;

    fn make_cfg(root: &Path, mode: crate::OperatingMode) -> StreamSpeculatorConfig {
        StreamSpeculatorConfig {
            mode,
            cwd: root.to_path_buf(),
            canonical_workspace: std::fs::canonicalize(root).ok(),
            max_read_calls: 16,
            tools: ToolRegistry::default(),
            code_intel: None,
            spec_cancel: CancellationToken::new(),
            run_cancel: None,
            sensitive_values: vec!["TOP_SECRET_CRED".to_owned()],
        }
    }

    #[test]
    fn incremental_json_path_extractor_handles_split_fragments_and_patch_files() {
        let mut buf = StreamingCallBuffer::new("c1".into(), "patch".into());
        assert_eq!(extract_paths_from_buffer(&mut buf).1, Vec::<String>::new());

        buf.args.push_str(r#"{"files":[{"pa"#);
        assert_eq!(extract_paths_from_buffer(&mut buf).1, Vec::<String>::new());

        buf.args.push_str(r#"th":"src/al"#);
        assert_eq!(extract_paths_from_buffer(&mut buf).1, Vec::<String>::new());

        buf.args.push_str(r#"pha.rs","edits":[]},{"path":"src/beta.rs"#);
        assert_eq!(
            extract_paths_from_buffer(&mut buf).1,
            vec!["src/alpha.rs".to_owned()]
        );

        buf.args.push_str(r#"","edits":[]}]}"#);
        assert_eq!(
            extract_paths_from_buffer(&mut buf).1,
            vec!["src/beta.rs".to_owned()]
        );
    }

    #[test]
    fn path_extractor_survives_multibyte_text_at_its_resume_offsets() {
        // The scan resumes 7 bytes (24 for programs) before the end of what
        // arrived: that offset must never land inside a character.
        let mut buf = StreamingCallBuffer::new("c1".into(), "mcp".into());
        buf.args.push_str(r#"{"value":"descriç123456"#);
        assert_eq!(extract_paths_from_buffer(&mut buf).1, Vec::<String>::new());
        buf.args.push_str(r#"","path":"src/a.rs"}"#);
        assert_eq!(extract_paths_from_buffer(&mut buf).1, vec!["src/a.rs".to_owned()]);

        let mut program = StreamingCallBuffer::new("c2".into(), "codemode".into());
        program.args.push_str(&format!(r#"{{"code":"// ç{}"#, "x".repeat(23)));
        assert_eq!(extract_paths_from_buffer(&mut program).1, Vec::<String>::new());
        // A 256-byte window after the call that ends inside a character.
        program.args.push_str(&format!("slim.read('src/b.rs'){}ção", " ".repeat(244)));
        assert_eq!(extract_paths_from_buffer(&mut program).1, vec!["src/b.rs".to_owned()]);
    }

    #[test]
    fn readonly_git_allowlist_accepts_safe_queries_and_rejects_injection() {
        assert!(is_allowlisted_readonly_git(r#"{"command":"git status -s"}"#));
        assert!(is_allowlisted_readonly_git(
            r#"{"command":"git","args":["log","-n","5","--oneline"]}"#
        ));
        assert!(is_allowlisted_readonly_git(
            r#"{"command":"git branch --show-current"}"#
        ));
        assert!(!is_allowlisted_readonly_git(
            r#"{"command":"git status; rm -rf /"}"#
        ));
        assert!(!is_allowlisted_readonly_git(
            r#"{"command":"git branch new-feature"}"#
        ));
        assert!(!is_allowlisted_readonly_git(
            r#"{"command":"git -c core.fsmonitor=evil status"}"#
        ));
        assert!(!is_allowlisted_readonly_git(
            r#"{"command":"git status","background":true}"#
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiwire_completion_and_stage3_hazard_tracker_work() {
        let root = TempRoot::new("spec-v2-multiwire");
        std::fs::write(root.join("a.txt"), "AAA\n").unwrap();
        std::fs::write(root.join("b.txt"), "BBB\n").unwrap();
        std::fs::write(root.join("c.txt"), "CCC\n").unwrap();

        let mut spec = StreamSpeculator::new(make_cfg(&root, crate::OperatingMode::Auto));

        // 1) Anthropic wire: patch a.txt completes cleanly (Stage 3 tracks mutated path `a.txt`).
        spec.on_event(&ProviderEvent::ToolCallStart {
            index: 0,
            id: "call_patch".into(),
            name: "patch".into(),
        });
        spec.on_event(&ProviderEvent::ToolCallInputDelta {
            index: 0,
            partial_json: r#"{"path":"a.txt","edits":[{"expected":"AAA","replacement":"AAA2"}]}"#.into(),
        });
        spec.on_event(&ProviderEvent::ContentBlockStop { index: 0 });

        // 2) Chat wire: disjoint read b.txt closes brace cleanly -> DISPATCHED!
        spec.on_event(&ProviderEvent::ToolCallDelta {
            index: Some(1),
            id: Some("call_read_b".into()),
            name: Some("read".into()),
            arguments: r#"{"path":"b.txt"}"#.into(),
        });

        // 3) Responses wire: overlapping read a.txt -> MUST BE BLOCKED by Stage 3!
        spec.on_event(&ProviderEvent::ToolCallComplete {
            index: 2,
            id: "call_read_a".into(),
            name: "read".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
        });

        // 4) Search after mutation -> MUST BE BLOCKED by Stage 3!
        spec.on_event(&ProviderEvent::ToolCallComplete {
            index: 3,
            id: "call_search".into(),
            name: "search".into(),
            arguments: r#"{"query":"BBB"}"#.into(),
        });

        let (entries, _) = spec.finish(true);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "call_read_b");
        let (_, entry) = entries.into_iter().next().unwrap();
        let outcome = entry.handle.await.unwrap().expect("read_b outcome");
        match outcome {
            SpeculativeOutcome::Native { outcome, .. } => {
                assert_eq!(outcome.result.output, "BBB\n");
            }
            _ => panic!("expected Native outcome"),
        }
    }

    #[tokio::test]
    async fn sensitive_secret_in_stream_aborts_speculation() {
        let root = TempRoot::new("spec-v2-secret");
        std::fs::write(root.join("normal.txt"), "hi\n").unwrap();
        let mut spec = StreamSpeculator::new(make_cfg(&root, crate::OperatingMode::Auto));
        spec.on_event(&ProviderEvent::ToolCallComplete {
            index: 0,
            id: "c1".into(),
            name: "read".into(),
            arguments: r#"{"path":"normal.txt"}"#.into(),
        });
        spec.on_event(&ProviderEvent::ToolCallComplete {
            index: 1,
            id: "c2".into(),
            name: "read".into(),
            arguments: r#"{"path":"TOP_SECRET_CRED"}"#.into(),
        });
        let (entries, _) = spec.finish(true);
        assert!(entries.is_empty(), "secret gate must abort and discard all speculative tasks");
    }

    #[test]
    fn codemode_path_extractor_handles_json_escaped_double_quotes_and_code_intel_lsp() {
        let mut buf = StreamingCallBuffer::new("c_esc".into(), "codemode".into());
        buf.args.push_str(
            r#"{"code":"await Promise.all([slim.read({path:\"src/app.rs\"}), slim.readMany([\"src/lib.rs\"]), slim.outlineMany({paths:[\"src/mod.rs\"]}), slim.patch({path:\"src/edit.rs\"}), slim.code_intel({action:\"definition\", path:\"src/ci.rs\"})]);"}"#,
        );
        let (allow_lsp, paths) = extract_paths_from_buffer(&mut buf);
        assert!(allow_lsp, "slim.code_intel( should enable allow_lsp_spawn");
        for expected in [
            "src/app.rs",
            "src/lib.rs",
            "src/mod.rs",
            "src/edit.rs",
            "src/ci.rs",
        ] {
            assert!(
                paths.contains(&expected.to_owned()),
                "missing `{expected}` in {paths:?}"
            );
        }
    }
}
