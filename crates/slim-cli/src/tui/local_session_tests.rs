use std::path::Path;

use slim_core::session::{
    DurableOperation, DurableOperationKind, DurableOutcome, DurableRecord, DurableRepo,
    DurableSessionHeader, JsonlRepo, ManualDrive, ManualExecutor, ManualRunSpec, ProviderResponse,
};

use super::select_previous_tui_session;

#[test]
fn restored_tasks_repopulate_the_dock_without_execution() {
    use slim_core::runtime::RuntimeCapabilityBridge;
    use slim_core::session::{
        AuthorizationGrant, CapabilityCatalog, TaskMutation, TaskMutationRequest,
    };
    let root = std::env::temp_dir().join(format!(
        "slim-todo-dock-{}-{}",
        std::process::id(),
        super::system_time_nanos(std::time::SystemTime::now())
    ));
    let mut bridge = RuntimeCapabilityBridge::new(
        JsonlRepo::create(
            root.join("session.jsonl"),
            DurableSessionHeader::new("todo", "now", root.to_string_lossy(), None, None),
        )
        .unwrap(),
        CapabilityCatalog::with_native_tools(),
    )
    .unwrap();
    bridge
        .apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "saved-todo".into(),
                entity_id: "session".into(),
                revision: 1,
                mutation: TaskMutation::TodoAdd {
                    title: "retained pending task".into(),
                    status: None,
                },
            },
            slim_core::OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .unwrap();
    let preflight = slim_core::session::SessionPreflight::from_open_repo(bridge.service().repo());
    let event = super::restored_todo_event(&preflight).unwrap();
    let mut app = slim_tui::app::AppState::new();
    let effects =
        slim_tui::reducer::reduce(&mut app, slim_tui::reducer::Action::UiEventReceived(event));
    assert_eq!(app.todo_items.len(), 1);
    assert_eq!(app.todo_items[0].title, "retained pending task");
    assert!(app.todo_dock_open);
    assert!(effects
        .iter()
        .all(|effect| matches!(effect, slim_tui::reducer::Effect::RequestRender)));
    drop(bridge);
    std::fs::remove_dir_all(root).unwrap();
}

struct FixtureExecutor;
struct FailingExecutor;

impl ManualExecutor for FixtureExecutor {
    type Error = std::io::Error;

    fn execute(
        &mut self,
        _effect: &slim_core::session::Effect,
    ) -> Result<ProviderResponse, Self::Error> {
        Ok(ProviderResponse::new("previous answer", None))
    }
}

impl ManualExecutor for FailingExecutor {
    type Error = std::io::Error;

    fn execute(
        &mut self,
        _effect: &slim_core::session::Effect,
    ) -> Result<ProviderResponse, Self::Error> {
        Err(std::io::Error::other("fixture failure"))
    }
}

fn create_completed(path: &Path, id: &str, cwd: &Path) {
    let cwd = std::fs::canonicalize(cwd).expect("canonical cwd");
    let header = DurableSessionHeader::new(id, "1", cwd.to_str().expect("unicode cwd"), None, None);
    let mut repo = JsonlRepo::create(path, header).expect("create session");
    ManualDrive::new(&mut repo, &mut FixtureExecutor)
        .run(ManualRunSpec::new(
            format!("{id}-operation"),
            format!("{id}-attempt"),
            format!("{id}-user"),
            format!("{id}-assistant"),
            "previous question",
            0,
        ))
        .expect("complete turn");
}

fn create_failed_terminal(path: &Path, id: &str, cwd: &Path) {
    let cwd = std::fs::canonicalize(cwd).expect("canonical cwd");
    let header = DurableSessionHeader::new(id, "3", cwd.to_str().expect("unicode cwd"), None, None);
    let mut repo = JsonlRepo::create(path, header).expect("create failed session");
    let operation_id = format!("{id}-operation");
    let result = ManualDrive::new(&mut repo, &mut FailingExecutor).run(ManualRunSpec::new(
        operation_id.clone(),
        format!("{id}-attempt"),
        format!("{id}-user"),
        format!("{id}-assistant"),
        "failed question",
        0,
    ));
    assert!(result.is_err());
    let seq = repo.next_seq().expect("next failed seq");
    repo.append(DurableRecord::Operation {
        seq,
        operation: DurableOperation {
            operation_id,
            kind: DurableOperationKind::Finished {
                outcome: DurableOutcome::Failed,
            },
        },
    })
    .expect("finish failed operation");
}

#[test]
fn previous_session_selection_stays_in_the_current_workspace() {
    let root = std::env::temp_dir().join(format!(
        "slim-local-resume-{}-{}",
        std::process::id(),
        super::system_time_nanos(std::time::SystemTime::now())
    ));
    let foreign = root.with_extension("foreign");
    std::fs::create_dir_all(root.join(".slim/sessions")).expect("session directory");
    std::fs::create_dir_all(&foreign).expect("foreign directory");
    let sessions = root.join(".slim/sessions");
    create_completed(&sessions.join("tui-valid.jsonl"), "tui-valid", &root);
    create_completed(&sessions.join("tui-foreign.jsonl"), "tui-foreign", &foreign);
    let current = sessions.join("tui-current.jsonl");
    let current_header = DurableSessionHeader::new(
        "tui-current",
        "2",
        std::fs::canonicalize(&root)
            .expect("canonical root")
            .to_str()
            .expect("unicode root"),
        None,
        None,
    );
    drop(JsonlRepo::create(&current, current_header).expect("current session"));
    create_failed_terminal(&sessions.join("tui-failed.jsonl"), "tui-failed", &root);

    let selected = select_previous_tui_session(&root, Some(&current))
        .expect("selection")
        .expect("previous session");
    assert_eq!(selected.preflight.session_id.as_deref(), Some("tui-valid"));
    assert_eq!(selected.history[0].content, "previous question");
    assert_eq!(selected.history[1].content, "previous answer");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&foreign);
}

#[test]
fn resume_after_cancel_restores_final_and_partial_answers_without_mutation() {
    struct CancelledExecutor;
    impl ManualExecutor for CancelledExecutor {
        type Error = std::io::Error;
        fn execute(
            &mut self,
            _: &slim_core::session::Effect,
        ) -> Result<ProviderResponse, Self::Error> {
            Ok(ProviderResponse::with_outcome(
                "partial answer",
                None,
                DurableOutcome::Cancelled,
            ))
        }
    }
    let root = std::env::temp_dir().join(format!(
        "slim-resume-cancel-{}-{}",
        std::process::id(),
        super::system_time_nanos(std::time::SystemTime::now())
    ));
    let sessions = root.join(".slim/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("tui-cancelled.jsonl");
    create_completed(&path, "tui-cancelled", &root);
    let mut repo = JsonlRepo::open_no_repair(&path).unwrap();
    let seq = repo.next_seq().unwrap();
    ManualDrive::new(&mut repo, &mut CancelledExecutor)
        .run(ManualRunSpec::new(
            "cancelled",
            "cancelled-attempt",
            "cancelled-input",
            "cancelled-answer",
            "cancelled question",
            seq,
        ))
        .unwrap();
    drop(repo);
    let before = std::fs::read(&path).unwrap();
    let selected = select_previous_tui_session(&root, None)
        .expect("resume cancelled session")
        .unwrap();
    let messages = super::session_transcript(&selected.preflight).unwrap();
    let mut app = slim_tui::app::AppState::new();
    let effects = slim_tui::reducer::reduce(
        &mut app,
        slim_tui::reducer::Action::UiEventReceived(slim_tui::api::UiEvent::SessionRestored {
            session_id: slim_tui::api::SessionId("tui-cancelled".into()),
            cwd: root.to_string_lossy().into_owned(),
            messages,
            skill_names: Vec::new(),
        }),
    );
    assert!(
        effects
            .iter()
            .all(|effect| matches!(effect, slim_tui::reducer::Effect::RequestRender)),
        "restore must only request rendering"
    );
    let frame = slim_tui::render::render(&app, 100, 30).lines.join("\n");
    for text in [
        "previous question",
        "previous answer",
        "cancelled question",
        "partial answer",
    ] {
        assert!(frame.contains(text), "missing {text}: {frame}");
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    std::fs::remove_dir_all(root).unwrap();
}
