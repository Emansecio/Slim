//! Session management beside the durable journals: the `/rename` title
//! sidecar, the `/resume` listing and the `/rewind` fork. Journals are only
//! read (headers, heads); the sidecar and the rewind child are the only files
//! written, both inside the directory of the session they belong to.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use slim_core::session::{
    fork_session_before_turn, list_turns, peek_session, preflight_session, DurableRecord,
    DurableSessionHeader, TurnError, TurnInfo,
};
use slim_tui::api::{SessionListItem, TurnListItem, UiCommand, UiEvent};

use super::{
    read_session_header, selected_from_preflight, switch_session, system_time_nanos,
    tui_session_id, workspace_sessions_dir, EventSink, SkillNameMemo, TuiStartup, UserShellContext,
};

/// Terminal cells a session title may take (the rail and picker show it on one line).
const TITLE_CELLS: usize = 80;
/// A sidecar is a few dozen bytes; anything larger is not ours.
const META_MAX_BYTES: u64 = 8 * 1024;
/// Most recent sessions offered by `/resume`.
const LIST_LIMIT: usize = 50;
/// Head of each journal read for the first prompt; never the whole file.
const PEEK_HEAD_BYTES: usize = 64 * 1024;

static NEXT_META_TEMP: AtomicU64 = AtomicU64::new(1);

/// A durable session of this workspace that automatic and explicit resume may
/// consider. Only the header was decoded, not the records.
pub(super) struct SessionCandidate {
    /// Canonical path inside the canonical sessions directory.
    pub(super) path: PathBuf,
    pub(super) header: DurableSessionHeader,
    /// Last modification, Unix epoch nanoseconds (falls back to `created`).
    pub(super) modified: u128,
    pub(super) created: u128,
    pub(super) bytes: u64,
}

impl SessionCandidate {
    /// Oldest-first ordering key; ties resolve by creation, id and path.
    pub(super) fn order_key(&self) -> (u128, u128, &str, &Path) {
        (
            self.modified,
            self.created,
            self.header.id.as_str(),
            self.path.as_path(),
        )
    }
}

/// Scans `<workspace>/.slim/sessions` for resumable-looking sessions: regular,
/// non-symlink `.jsonl` files inside the canonical sessions directory whose
/// header decodes, whose id starts with `tui-` and equals the file stem, and
/// whose header cwd is this workspace.
///
/// Header-only: a full preflight parses every record line, which would make
/// the scan O(all session bytes) as sessions accumulate. The strict header
/// decode already rejects non-v2 schema versions.
pub(super) fn scan_tui_sessions(workspace_root: &Path) -> Result<Vec<SessionCandidate>, String> {
    let canonical_workspace =
        fs::canonicalize(workspace_root).map_err(|error| format!("workspace path: {error}"))?;
    let Some(sessions) = workspace_sessions_dir(&canonical_workspace, false)? else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();

    for entry in fs::read_dir(&sessions).map_err(|error| format!("read sessions: {error}"))? {
        let entry = entry.map_err(|error| format!("read session entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("session entry type: {error}"))?;
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let canonical_path = match fs::canonicalize(&path) {
            Ok(path) if path.starts_with(&sessions) => path,
            _ => continue,
        };
        let Some(header) = read_session_header(&canonical_path) else {
            continue;
        };
        if !header.id.starts_with("tui-")
            || canonical_path.file_stem().and_then(|stem| stem.to_str()) != Some(header.id.as_str())
        {
            continue;
        }
        let header_cwd = match fs::canonicalize(&header.cwd) {
            Ok(cwd) => cwd,
            Err(_) => continue,
        };
        if header_cwd != canonical_workspace {
            continue;
        }
        let created = header.timestamp.parse::<u128>().unwrap_or(0);
        let metadata = fs::metadata(&canonical_path).ok();
        let modified = metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .map(system_time_nanos)
            .unwrap_or(created);
        candidates.push(SessionCandidate {
            path: canonical_path,
            header,
            modified,
            created,
            bytes: metadata.map_or(0, |metadata| metadata.len()),
        });
    }
    Ok(candidates)
}

/// `tui-` followed by a non-empty run of `[A-Za-z0-9_-]`: what
/// `create_tui_session` generates, and nothing that could name another path.
pub(super) fn valid_tui_session_id(id: &str) -> bool {
    id.len() <= 128
        && id.strip_prefix("tui-").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
}

/// True when another process holds the session's write lock. Probes the
/// existing `<file>.lock` without creating it, and releases at once, so
/// nothing is stolen, kept or left behind. Concurrent probes do not exclude
/// each other (shared lock, shared access), unlike the writer's exclusive one.
pub(super) fn session_in_use(path: &Path) -> bool {
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let mut options = fs::OpenOptions::new();
    options.write(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 0x1 | 0x2);
    match options.open(&lock_path) {
        Ok(file) => !matches!(file.try_lock_shared(), Ok(())),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Single line without control or invisible formatting characters (an escape
/// sequence in a saved prompt must not reach the terminal).
fn collapse_line(raw: &str) -> String {
    raw.chars()
        .filter(|character| {
            !matches!(
                character,
                '\u{200B}'..='\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2060}'..='\u{2064}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{FEFF}'
            )
        })
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Title as stored and shown: one line, at most [`TITLE_CELLS`] cells.
/// Empty means "no title".
pub(super) fn clean_session_title(raw: &str) -> String {
    slim_tui::picker::truncate_cells(&collapse_line(raw), TITLE_CELLS)
}

/// `<stem>.meta.json` beside the journal. It never ends in `.jsonl`, so the
/// session scan cannot mistake it for a session.
fn meta_path(session_path: &Path) -> Option<PathBuf> {
    let mut name = session_path.file_stem()?.to_os_string();
    name.push(".meta.json");
    Some(session_path.with_file_name(name))
}

#[derive(Serialize, Deserialize)]
struct SessionMeta {
    title: Option<String>,
    updated_ns: u64,
}

/// The session's title, or `None` when there is none, the sidecar is missing,
/// corrupt, oversized or not a regular file. Never an error.
pub(super) fn read_session_title(session_path: &Path) -> Option<String> {
    let path = meta_path(session_path)?;
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > META_MAX_BYTES {
        return None;
    }
    let meta: SessionMeta = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
    let title = clean_session_title(&meta.title?);
    (!title.is_empty()).then_some(title)
}

/// Stores `title` (already cleaned) for the session; empty removes the sidecar.
/// The write goes through a temp file in the same directory and a rename, so a
/// reader sees the old or the new sidecar, never a partial one.
pub(super) fn write_session_title(session_path: &Path, title: &str) -> Result<(), String> {
    let path = meta_path(session_path).ok_or("caminho da sessão inválido")?;
    if title.is_empty() {
        return match fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(format!(
                "não foi possível remover o nome da sessão: {error}"
            )),
            _ => Ok(()),
        };
    }
    let updated_ns = u64::try_from(system_time_nanos(SystemTime::now())).unwrap_or(u64::MAX);
    let encoded = serde_json::to_vec(&SessionMeta {
        title: Some(title.to_owned()),
        updated_ns,
    })
    .map_err(|error| error.to_string())?;
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT_META_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &path)
    })();
    written.map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("não foi possível salvar o nome da sessão: {error}")
    })
}

/// The `/resume` list: the most recent sessions of the workspace with what a
/// picker needs, read from headers, file heads and sidecars only. Sessions with
/// neither a prompt nor a title (empty ones) are left out.
pub(super) fn list_workspace_sessions(
    workspace_root: &Path,
    current: Option<&Path>,
) -> Result<Vec<SessionListItem>, String> {
    let mut candidates = scan_tui_sessions(workspace_root)?;
    candidates.sort_by(|left, right| right.order_key().cmp(&left.order_key()));
    let current = current.and_then(|path| fs::canonicalize(path).ok());
    let mut items = Vec::new();
    for candidate in candidates {
        if items.len() == LIST_LIMIT {
            break;
        }
        let title = read_session_title(&candidate.path);
        let first_prompt = peek_session(&candidate.path, PEEK_HEAD_BYTES)
            .ok()
            .and_then(|peek| peek.first_prompt)
            .map(|prompt| collapse_line(&prompt))
            .unwrap_or_default();
        if first_prompt.is_empty() && title.is_none() {
            continue;
        }
        let is_current = current.as_deref() == Some(candidate.path.as_path());
        items.push(SessionListItem {
            in_use: !is_current && session_in_use(&candidate.path),
            current: is_current,
            id: candidate.header.id,
            title,
            first_prompt,
            updated_ms: u64::try_from(candidate.modified / 1_000_000).unwrap_or(u64::MAX),
            bytes: candidate.bytes,
        });
    }
    Ok(items)
}

/// The `/rewind` list: the finished turns of the session at `path`.
pub(super) fn list_finished_turns(path: &Path) -> Result<Vec<TurnListItem>, String> {
    let turns =
        list_turns(path).map_err(|error| format!("Não foi possível ler a sessão: {error}"))?;
    let items: Vec<TurnListItem> = turns
        .into_iter()
        .filter(|turn| turn.terminal)
        .map(
            |TurnInfo {
                 index,
                 first_seq,
                 prompt,
                 ..
             }| TurnListItem {
                index,
                first_seq,
                prompt,
            },
        )
        .collect();
    if items.is_empty() {
        return Err("Nenhuma conversa salva nesta sessão".into());
    }
    Ok(items)
}

/// A child session cut just before a turn.
pub(super) struct RewoundSession {
    pub(super) child: PathBuf,
    /// Turns the child no longer has (the chosen one and every later one).
    pub(super) removed: usize,
    /// Full text of the chosen turn's prompt, to hand back to the composer.
    pub(super) draft: String,
}

/// Forks the session at `current` before the finished turn whose user entry
/// has sequence `first_seq`. The original file is left untouched.
pub(super) fn fork_rewind_child(current: &Path, first_seq: u64) -> Result<RewoundSession, String> {
    let turns =
        list_turns(current).map_err(|error| format!("Não foi possível ler a sessão: {error}"))?;
    let turn = turns
        .iter()
        .find(|turn| turn.first_seq == first_seq)
        .ok_or("Turno não encontrado nesta sessão")?;
    if !turn.terminal {
        return Err("Este turno ainda não terminou".into());
    }
    let draft = preflight_session(current)
        .ok()
        .and_then(|preflight| {
            preflight
                .records
                .into_iter()
                .find_map(|record| match record {
                    DurableRecord::Entry { seq, entry } if seq == first_seq => Some(entry.content),
                    _ => None,
                })
        })
        .unwrap_or_else(|| turn.prompt.clone());
    let timestamp = system_time_nanos(SystemTime::now());
    for _ in 0..16 {
        let child_id = tui_session_id(timestamp);
        match fork_session_before_turn(current, turn.index, &child_id) {
            Ok(child) => {
                return Ok(RewoundSession {
                    child,
                    removed: turns.len() - turn.index,
                    draft,
                });
            }
            Err(TurnError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(error) => return Err(format!("Não foi possível voltar nesta sessão: {error}")),
        }
    }
    Err("Não foi possível alocar um arquivo de sessão".into())
}

/// Unix epoch milliseconds now, for the host clock the TUI renders ages from.
pub(super) fn now_ms() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

fn notify(sink: &EventSink, message: impl Into<String>) {
    let _ = sink.send(UiEvent::Notification {
        message: message.into(),
    });
}

/// Runs `work` on its own thread and sends the event it returns; if the
/// thread cannot start, sends `unavailable` instead, so a request is always
/// answered (disk work stays off the worker loop, like `serve_workspace_files`).
fn answer_off_loop(
    name: &str,
    sink: &EventSink,
    work: impl FnOnce() -> UiEvent + Send + 'static,
    unavailable: UiEvent,
) {
    let answer = sink.clone();
    let fallback = sink.clone();
    let spawned = thread::Builder::new().name(name.into()).spawn(move || {
        let _ = answer.send(work());
    });
    if spawned.is_err() {
        let _ = fallback.send(unavailable);
    }
}

/// True for the commands that manage the session itself; the worker refuses
/// them whenever it is not idle.
pub(super) fn is_session_command(command: &UiCommand) -> bool {
    matches!(
        command,
        UiCommand::RenameSession { .. }
            | UiCommand::ListSessions { .. }
            | UiCommand::ResumeSession { .. }
            | UiCommand::ListTurns { .. }
            | UiCommand::RewindSession { .. }
    )
}

/// Answers a session command the worker cannot serve now. `wait` says why
/// ("Aguarde ou cancele a execução"); lists answer with an error, the rest
/// with a notification, so the TUI never waits on a request that will not
/// come back.
pub(super) fn refuse_session_command(sink: &EventSink, command: &UiCommand, wait: &str) {
    match command {
        UiCommand::ListSessions { request_id } => {
            let _ = sink.send(UiEvent::SessionsListed {
                request_id: *request_id,
                now_ms: now_ms(),
                items: Vec::new(),
                error: Some(format!("{wait} antes de listar as sessões")),
            });
        }
        UiCommand::ListTurns { request_id } => {
            let _ = sink.send(UiEvent::TurnsListed {
                request_id: *request_id,
                items: Vec::new(),
                error: Some(format!("{wait} antes de listar os turnos")),
            });
        }
        UiCommand::RenameSession { .. } => {
            notify(sink, format!("{wait} antes de renomear a sessão"));
        }
        UiCommand::ResumeSession { .. } => {
            notify(sink, format!("{wait} antes de retomar outra sessão"));
        }
        UiCommand::RewindSession { .. } => {
            notify(sink, format!("{wait} antes de voltar a um turno"));
        }
        _ => {}
    }
}

/// `/rename`: stores the title beside the session, or keeps it until the
/// session exists. Empty clears the name.
pub(super) fn rename_session(startup: &mut TuiStartup, sink: &EventSink, raw_title: &str) {
    let title = clean_session_title(raw_title);
    if let Some(path) = startup.resume_path.as_deref() {
        if let Err(message) = write_session_title(path, &title) {
            notify(sink, message);
            return;
        }
    } else {
        startup.pending_session_title = (!title.is_empty()).then(|| title.clone());
    }
    if title.is_empty() {
        let _ = sink.send(UiEvent::SessionTitleChanged { title: None });
        notify(sink, "Nome da sessão removido");
    } else {
        notify(sink, format!("Sessão renomeada: {title}"));
        let _ = sink.send(UiEvent::SessionTitleChanged { title: Some(title) });
    }
}

/// `/resume` list, built off the worker loop.
pub(super) fn serve_session_list(startup: &TuiStartup, sink: &EventSink, request_id: u64) {
    let root = startup.options.workspace_root.clone();
    let current = startup.resume_path.clone();
    let listed = move || {
        let (items, error) = match root {
            Some(root) => match list_workspace_sessions(&root, current.as_deref()) {
                Ok(items) => (items, None),
                Err(message) => (
                    Vec::new(),
                    Some(format!("Não foi possível listar as sessões: {message}")),
                ),
            },
            None => (
                Vec::new(),
                Some("Diretório de trabalho indisponível".to_owned()),
            ),
        };
        UiEvent::SessionsListed {
            request_id,
            now_ms: now_ms(),
            items,
            error,
        }
    };
    answer_off_loop(
        "slim-session-list",
        sink,
        listed,
        UiEvent::SessionsListed {
            request_id,
            now_ms: now_ms(),
            items: Vec::new(),
            error: Some("Não foi possível listar as sessões".into()),
        },
    );
}

/// `/rewind` list: the current session's finished turns, built off the worker loop.
pub(super) fn serve_turn_list(startup: &TuiStartup, sink: &EventSink, request_id: u64) {
    let current = startup.resume_path.clone();
    let listed = move || {
        let (items, error) = match current {
            Some(path) => match list_finished_turns(&path) {
                Ok(items) => (items, None),
                Err(message) => (Vec::new(), Some(message)),
            },
            None => (
                Vec::new(),
                Some("Nenhuma conversa salva nesta sessão".to_owned()),
            ),
        };
        UiEvent::TurnsListed {
            request_id,
            items,
            error,
        }
    };
    answer_off_loop(
        "slim-turn-list",
        sink,
        listed,
        UiEvent::TurnsListed {
            request_id,
            items: Vec::new(),
            error: Some("Não foi possível listar os turnos".into()),
        },
    );
}

/// `ResumeSession`: switches to the workspace session named `id`. The id is
/// checked before it can name a path and must then be one of the scanner's
/// candidates, so header, stem and workspace checks apply exactly as they do
/// for automatic resume.
pub(super) fn resume_session_by_id(
    startup: &mut TuiStartup,
    sink: &EventSink,
    skill_memo: &mut SkillNameMemo,
    shell_context: &UserShellContext,
    id: &str,
) {
    if !valid_tui_session_id(id) {
        notify(sink, "Identificador de sessão inválido");
        return;
    }
    let workspace = startup
        .options
        .workspace_root
        .clone()
        .expect("workspace root initialized");
    let candidates = match scan_tui_sessions(&workspace) {
        Ok(candidates) => candidates,
        Err(message) => {
            notify(sink, message);
            return;
        }
    };
    let Some(candidate) = candidates
        .into_iter()
        .find(|candidate| candidate.header.id == id)
    else {
        notify(sink, "Sessão não encontrada neste diretório");
        return;
    };
    let current = startup
        .resume_path
        .as_deref()
        .and_then(|path| fs::canonicalize(path).ok());
    if current.as_deref() == Some(candidate.path.as_path()) {
        notify(sink, "Esta já é a sessão atual");
        return;
    }
    if session_in_use(&candidate.path) {
        notify(sink, "A sessão está em uso por outro processo");
        return;
    }
    let selected = match preflight_session(&candidate.path)
        .map_err(|error| format!("{REFUSED}: {error}"))
        .and_then(|preflight| selected_from_preflight(preflight, REFUSED))
    {
        Ok(selected) => selected,
        Err(message) => {
            notify(sink, message);
            return;
        }
    };
    if selected.preflight.summary.terminal_operation_ids.is_empty() {
        notify(sink, "A sessão não tem conversa concluída para retomar");
        return;
    }
    switch_session(
        startup,
        sink,
        skill_memo,
        shell_context,
        selected,
        "Sessão retomada. Envie um prompt para continuar.",
    );
}

/// `RewindSession`: forks the current session before the chosen turn, switches
/// to the fork and hands the turn's prompt back as a draft. The original file
/// stays as it was; files the removed turns changed are not restored.
pub(super) fn rewind_current_session(
    startup: &mut TuiStartup,
    sink: &EventSink,
    skill_memo: &mut SkillNameMemo,
    shell_context: &UserShellContext,
    first_seq: u64,
) {
    let Some(current) = startup.resume_path.clone() else {
        notify(sink, "Nenhuma conversa salva nesta sessão");
        return;
    };
    let rewound = match fork_rewind_child(&current, first_seq) {
        Ok(rewound) => rewound,
        Err(message) => {
            notify(sink, message);
            return;
        }
    };
    // The fork is a new file: keep the name the user gave the conversation.
    if let Some(title) = read_session_title(&current) {
        if let Err(message) = write_session_title(&rewound.child, &title) {
            notify(sink, message);
        }
    }
    // Unlike automatic selection, an empty child (rewound to the first turn) is valid.
    let selected = match preflight_session(&rewound.child)
        .map_err(|error| format!("{REFUSED}: {error}"))
        .and_then(|preflight| selected_from_preflight(preflight, REFUSED))
    {
        Ok(selected) => selected,
        Err(message) => {
            notify(sink, message);
            return;
        }
    };
    let notice = format!(
        "Voltou {} turno(s). Arquivos alterados não foram restaurados; veja Ctrl+D.",
        rewound.removed
    );
    // After `SessionRestored`, which resets the composer.
    if switch_session(startup, sink, skill_memo, shell_context, selected, &notice) {
        let _ = sink.send(UiEvent::RestoreDraft {
            text: rewound.draft,
        });
    }
}

const REFUSED: &str = "Sessão não pode ser retomada";
