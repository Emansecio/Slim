use slim_core::session::{
    drive_manual, fork_session_before_turn, list_turns, peek_session, preflight_session,
    provider_messages_from_records, CompactionCheckpoint, CompactionReason, DurableEntry,
    DurableEntryRole, DurableRecord, DurableRepo, DurableSessionHeader, Effect, JsonlRepo,
    ManualExecutor, ManualRunSpec, PreflightStatus, ProviderResponse, SessionFormat, TurnError,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Sessions(PathBuf);

impl Sessions {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let nonce = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "slim-session-turns-{}-{stamp}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("sessions directory");
        Self(directory)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.0.join(format!("{id}.jsonl"))
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Answer;

impl ManualExecutor for Answer {
    type Error = &'static str;

    fn execute(&mut self, effect: &Effect) -> Result<ProviderResponse, Self::Error> {
        let Effect::ProviderRequest { id, .. } = effect;
        Ok(ProviderResponse::new(format!("answer to {id:?}"), None))
    }
}

struct Fail;

impl ManualExecutor for Fail {
    type Error = &'static str;

    fn execute(&mut self, _effect: &Effect) -> Result<ProviderResponse, Self::Error> {
        Err("provider unavailable")
    }
}

fn header(id: &str) -> DurableSessionHeader {
    DurableSessionHeader::new(id, "1", std::env::temp_dir().to_string_lossy(), None, None)
}

/// Append one prompt the way a TUI prompt is persisted (operation id
/// `resume-{session}-{first_seq}`).
fn run_turn<E: ManualExecutor>(repo: &mut JsonlRepo, prompt: &str, executor: &mut E) {
    let first_seq = repo.next_seq().expect("next seq");
    let id = repo.header().id.clone();
    let operation = format!("resume-{id}-{first_seq}");
    let mut spec = ManualRunSpec::new(
        operation.clone(),
        format!("{operation}-attempt"),
        format!("{operation}-input"),
        format!("{operation}-assistant"),
        prompt,
        first_seq,
    );
    if let Some(parent) = repo.records().iter().rev().find_map(|record| match record {
        DurableRecord::Entry { entry, .. } => Some(entry.entry_id.clone()),
        _ => None,
    }) {
        spec = spec.with_parent_entry_id(parent);
    }
    let _ = drive_manual(repo, executor, spec);
}

fn session_with_turns(sessions: &Sessions, id: &str, prompts: &[&str]) -> PathBuf {
    let path = sessions.path(id);
    let mut repo = JsonlRepo::create(&path, header(id)).expect("create");
    for prompt in prompts {
        run_turn(&mut repo, prompt, &mut Answer);
    }
    drop(repo);
    path
}

/// Mirrors the gates `ensure_resume_preflight` applies in slim-cli (that
/// function is crate-private there): healthy v2 that can resume and holds no
/// pending, claimed or suspended work.
fn assert_resumable(path: &Path) -> slim_core::session::SessionPreflight {
    let report = preflight_session(path).expect("preflight");
    assert_eq!(report.format, Some(SessionFormat::DurableV2));
    assert_eq!(report.status, PreflightStatus::Healthy);
    assert!(report.can_resume_v2());
    assert_eq!(report.summary.pending_count(), 0);
    assert_eq!(report.summary.claimed_count(), 0);
    assert_eq!(report.summary.suspended_count(), 0);
    report
}

#[test]
fn list_turns_reports_three_terminal_prompts_in_order() {
    let sessions = Sessions::new();
    let path = session_with_turns(
        &sessions,
        "tui-list",
        &[
            "first prompt\nsecond line",
            "\n\n  second prompt  ",
            "third",
        ],
    );
    let turns = list_turns(&path).expect("turns");
    assert_eq!(turns.len(), 3);
    assert_eq!(
        turns
            .iter()
            .map(|turn| turn.prompt.as_str())
            .collect::<Vec<_>>(),
        ["first prompt", "second prompt", "third"]
    );
    assert!(turns.iter().all(|turn| turn.terminal));
    assert_eq!(
        turns.iter().map(|turn| turn.index).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(turns[0].first_seq, 0);
    assert!(turns[0].first_seq < turns[1].first_seq && turns[1].first_seq < turns[2].first_seq);
}

#[test]
fn list_turns_truncates_long_prompts_and_flags_unfinished_turns() {
    let sessions = Sessions::new();
    let path = sessions.path("tui-open");
    let mut repo = JsonlRepo::create(&path, header("tui-open")).expect("create");
    run_turn(&mut repo, &"é".repeat(500), &mut Answer);
    run_turn(&mut repo, "never finished", &mut Fail);
    drop(repo);
    let turns = list_turns(&path).expect("turns");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].prompt.chars().count(), 200);
    assert!(turns[0].prompt.ends_with('…'));
    assert!(turns[0].terminal);
    assert!(!turns[1].terminal);
}

#[test]
fn fork_before_turn_keeps_only_earlier_turns_and_leaves_the_parent_untouched() {
    let sessions = Sessions::new();
    let parent = session_with_turns(&sessions, "tui-parent", &["one", "two", "three"]);
    let before = fs::read(&parent).expect("parent bytes");
    let source = preflight_session(&parent).expect("source");
    let turns = list_turns(&parent).expect("turns");

    let child =
        fork_session_before_turn(&parent, 2, "tui-1-2-3").expect("fork before the third turn");

    assert_eq!(
        child.file_stem().and_then(|stem| stem.to_str()),
        Some("tui-1-2-3")
    );
    assert_eq!(
        child.parent(),
        parent.canonicalize().expect("parent").parent()
    );
    assert_eq!(fs::read(&parent).expect("parent bytes"), before);

    let report = assert_resumable(&child);
    let header = report.header.as_ref().expect("header");
    assert_eq!(header.id, "tui-1-2-3");
    assert_eq!(header.parent_id.as_deref(), Some("tui-parent"));
    assert_eq!(header.cwd, source.header.as_ref().expect("header").cwd);
    let cutoff = turns[2].first_seq - 1;
    assert_eq!(header.cutoff_seq, Some(cutoff));
    assert_eq!(report.last_seq, Some(cutoff));
    let expected: Vec<_> = source
        .records
        .iter()
        .filter(|record| record.seq() <= cutoff)
        .cloned()
        .collect();
    assert_eq!(report.records, expected);
    let messages = provider_messages_from_records(report.records.iter()).expect("messages");
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == "user")
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(messages.len(), 4);
    assert_eq!(list_turns(&child).expect("child turns").len(), 2);

    // The child continues from the cutoff like any resumed session.
    let mut repo = JsonlRepo::open(&child).expect("open child");
    assert_eq!(repo.next_seq().expect("next"), cutoff + 1);
    run_turn(&mut repo, "replacement", &mut Answer);
    drop(repo);
    assert_eq!(list_turns(&child).expect("turns").len(), 3);
    assert_eq!(fs::read(&parent).expect("parent bytes"), before);
}

#[test]
fn fork_works_while_the_parent_is_open_for_writing() {
    let sessions = Sessions::new();
    let id = "tui-live";
    let path = sessions.path(id);
    let mut repo = JsonlRepo::create(&path, header(id)).expect("create");
    run_turn(&mut repo, "one", &mut Answer);
    run_turn(&mut repo, "two", &mut Answer);
    let child = fork_session_before_turn(&path, 1, "tui-live-child").expect("fork");
    assert_eq!(assert_resumable(&child).records.len(), 6);
    run_turn(&mut repo, "three", &mut Answer);
    drop(repo);
    assert_eq!(list_turns(&path).expect("turns").len(), 3);
}

#[test]
fn fork_before_the_first_turn_yields_a_resumable_empty_child() {
    let sessions = Sessions::new();
    let parent = session_with_turns(&sessions, "tui-first", &["one", "two"]);
    let child = fork_session_before_turn(&parent, 0, "tui-first-child").expect("fork");
    let report = assert_resumable(&child);
    assert!(report.records.is_empty());
    let header = report.header.expect("header");
    assert_eq!(header.parent_id.as_deref(), Some("tui-first"));
    assert_eq!(header.cutoff_seq, None);
    assert!(list_turns(&child).expect("turns").is_empty());
}

#[test]
fn fork_keeps_compaction_checkpoints_inside_the_cutoff() {
    let sessions = Sessions::new();
    let id = "tui-compact";
    let path = sessions.path(id);
    let mut repo = JsonlRepo::create(&path, header(id)).expect("create");
    run_turn(&mut repo, "one", &mut Answer);
    let anchor = repo
        .records()
        .iter()
        .rev()
        .find_map(|record| match record {
            DurableRecord::Entry { entry, .. } => Some(entry.entry_id.clone()),
            _ => None,
        })
        .expect("assistant entry");
    let seq = repo.next_seq().expect("next");
    let checkpoint = |name: &str| CompactionCheckpoint {
        checkpoint_id: name.into(),
        summary: "summary".into(),
        first_kept_entry_id: anchor.clone(),
        prefix_fingerprint: "0123456789abcdef".into(),
        previous_checkpoint_id: None,
        tokens_before: 10,
        tokens_after: 5,
        input_tokens: None,
        output_tokens: None,
        duration_ms: 1,
        reason: CompactionReason::Branch,
        read_files: Vec::new(),
        modified_files: Vec::new(),
    };
    repo.append(DurableRecord::Compaction {
        seq,
        checkpoint: checkpoint("kept"),
    })
    .expect("checkpoint");
    run_turn(&mut repo, "two", &mut Answer);
    let later = repo.next_seq().expect("next");
    repo.append(DurableRecord::Compaction {
        seq: later,
        checkpoint: CompactionCheckpoint {
            previous_checkpoint_id: Some("kept".into()),
            ..checkpoint("dropped")
        },
    })
    .expect("later checkpoint");
    run_turn(&mut repo, "three", &mut Answer);
    drop(repo);

    let turns = list_turns(&path).expect("turns");
    let child = fork_session_before_turn(&path, 1, "tui-compact-child").expect("fork");
    let report = assert_resumable(&child);
    let checkpoints: Vec<_> = report
        .records
        .iter()
        .filter_map(|record| match record {
            DurableRecord::Compaction { checkpoint, .. } => Some(checkpoint.checkpoint_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(checkpoints, ["kept"]);
    assert!(report.last_seq.expect("last") < turns[1].first_seq);
}

#[test]
fn fork_refuses_unfinished_targets_and_open_work_before_the_cutoff() {
    let sessions = Sessions::new();
    let path = sessions.path("tui-mid");
    let mut repo = JsonlRepo::create(&path, header("tui-mid")).expect("create");
    run_turn(&mut repo, "finished", &mut Answer);
    run_turn(&mut repo, "unfinished", &mut Fail);
    drop(repo);
    assert!(matches!(
        fork_session_before_turn(&path, 1, "tui-mid-child"),
        Err(TurnError::TurnNotTerminal { index: 1 })
    ));
    assert!(!sessions.path("tui-mid-child").exists());

    // A later prompt after an abandoned operation: the cutoff before it would
    // leave the earlier operation open.
    let path = sessions.path("tui-open-before");
    let mut repo = JsonlRepo::create(&path, header("tui-open-before")).expect("create");
    run_turn(&mut repo, "unfinished", &mut Fail);
    run_turn(&mut repo, "finished later", &mut Answer);
    drop(repo);
    assert!(matches!(
        fork_session_before_turn(&path, 1, "tui-open-child"),
        Err(TurnError::OpenWorkBeforeTurn { index: 1 })
    ));
    assert!(!sessions.path("tui-open-child").exists());
}

#[test]
fn fork_refuses_bad_ids_out_of_range_turns_and_existing_children() {
    let sessions = Sessions::new();
    let parent = session_with_turns(&sessions, "tui-args", &["one", "two"]);
    for child in [
        "",
        ".",
        "..",
        "../escape",
        "nested/child",
        "nested\\child",
        "a b",
    ] {
        assert!(
            matches!(
                fork_session_before_turn(&parent, 1, child),
                Err(TurnError::InvalidChildId(_))
            ),
            "child={child:?}"
        );
    }
    assert!(matches!(
        fork_session_before_turn(&parent, 2, "tui-late"),
        Err(TurnError::TurnOutOfRange { index: 2, turns: 2 })
    ));
    fork_session_before_turn(&parent, 1, "tui-once").expect("first fork");
    assert!(matches!(
        fork_session_before_turn(&parent, 1, "tui-once"),
        Err(TurnError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert!(matches!(
        fork_session_before_turn(&sessions.path("missing"), 0, "tui-missing"),
        Err(TurnError::Io(_))
    ));
}

#[test]
fn legacy_v1_and_damaged_sessions_are_refused() {
    let sessions = Sessions::new();
    let v1 = sessions.path("tui-v1");
    fs::write(
        &v1,
        "{\"type\":\"session\",\"schema_version\":1,\"id\":\"tui-v1\",\"timestamp\":\"t\",\"cwd\":\"x\",\"parent_id\":null,\"cutoff_seq\":null}\n",
    )
    .expect("v1 fixture");
    assert!(matches!(list_turns(&v1), Err(TurnError::UnsupportedSchema)));
    assert!(matches!(
        fork_session_before_turn(&v1, 0, "tui-v1-child"),
        Err(TurnError::UnsupportedSchema)
    ));

    let torn = session_with_turns(&sessions, "tui-torn", &["one"]);
    let mut bytes = fs::read(&torn).expect("bytes");
    bytes.extend_from_slice(b"{\"type\":\"entry\",\"seq\":");
    fs::write(&torn, bytes).expect("torn tail");
    assert!(matches!(list_turns(&torn), Err(TurnError::NotHealthy(_))));

    let garbage = sessions.path("tui-garbage");
    fs::write(&garbage, "not json\n").expect("garbage");
    assert!(matches!(
        list_turns(&garbage),
        Err(TurnError::NotHealthy(_))
    ));
}

#[test]
fn peek_returns_the_first_prompt_without_reading_the_whole_file() {
    let sessions = Sessions::new();
    let id = "tui-big";
    let path = sessions.path(id);
    let mut repo = JsonlRepo::create(&path, header(id)).expect("create");
    run_turn(&mut repo, "\n  the very first prompt\nmore", &mut Answer);
    for index in 0..40 {
        let seq = repo.next_seq().expect("next");
        repo.append(DurableRecord::Entry {
            seq,
            entry: DurableEntry {
                entry_id: format!("filler-{index}"),
                role: DurableEntryRole::Assistant,
                content: "x".repeat(8 * 1024),
                parent_entry_id: None,
                operation_id: format!("filler-op-{index}"),
                tool_call_id: None,
                tool_calls: Vec::new(),
                content_blocks: Vec::new(),
            },
        })
        .expect("filler");
    }
    drop(repo);
    let length = fs::metadata(&path).expect("metadata").len();
    assert!(length > 64 * 1024, "fixture must exceed the head window");

    let peek = peek_session(&path, 64 * 1024).expect("peek");
    assert_eq!(peek.first_prompt.as_deref(), Some("the very first prompt"));
    assert_eq!(peek.turns_hint, None);

    // A window that cuts the first user line yields nothing instead of a
    // partial parse.
    let header_line = fs::read_to_string(&path)
        .expect("text")
        .lines()
        .next()
        .expect("header")
        .len();
    let cut = peek_session(&path, header_line + 10).expect("peek");
    assert_eq!(cut.first_prompt, None);
}

#[test]
fn peek_handles_small_unterminated_and_empty_files() {
    let sessions = Sessions::new();
    let path = session_with_turns(&sessions, "tui-small", &["only prompt"]);
    assert_eq!(
        peek_session(&path, 64 * 1024)
            .expect("peek")
            .first_prompt
            .as_deref(),
        Some("only prompt")
    );

    let unterminated = sessions.path("tui-unterminated");
    let bytes = fs::read(&path).expect("bytes");
    fs::write(&unterminated, &bytes[..bytes.len() - 1]).expect("no final newline");
    assert_eq!(
        peek_session(&unterminated, 64 * 1024)
            .expect("peek")
            .first_prompt
            .as_deref(),
        Some("only prompt")
    );

    let empty = sessions.path("tui-empty");
    fs::write(&empty, "").expect("empty");
    assert_eq!(peek_session(&empty, 1024).expect("peek").first_prompt, None);
    assert_eq!(
        peek_session(&sessions.path("tui-header-only-missing"), 1024)
            .expect_err("missing")
            .kind(),
        std::io::ErrorKind::NotFound
    );
}
