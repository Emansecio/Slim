use std::fs;
use std::sync::Arc;

use slim_cli::{run_fake_headless, ExitCode, HeadlessRequest};
use slim_core::agents::{ChildStatus, Scheduler, SpawnRequest, SpawnResult};
use slim_core::context::{compact, ArtifactStore, ContextItem};
use slim_core::mcp::McpCatalog;
use slim_core::runtime::PromptQueue;
use slim_core::session::{branch, recover, SessionWriter};
use slim_core::skills::{discover, read_body, SkillRoot};
use slim_core::task::{Assurance, Goal, Plan, TodoStatus, TodoTracker};
use slim_core::tools::{
    apply_exact_patch, read_file, search_literal, write_file, FilePrecondition,
};
use slim_core::{AppHandle, EventKind, FakeProvider, OperatingMode, SessionEvent};
use slim_tui::api::{SessionId, UiEvent};
use slim_tui::app::AppState;
use slim_tui::reducer::{reduce, Action};
use slim_tui::view_model::ViewModel;

#[test]
fn fake_v1_flow_covers_headless_core_and_tui_contracts() {
    let root = std::env::temp_dir().join(format!("slim-e2e-{}", std::process::id()));
    fs::create_dir_all(&root).expect("workspace");
    let source = root.join("source.txt");
    fs::write(&source, "alpha\nneedle\n").expect("source");

    let auto = run_fake_headless(HeadlessRequest {
        prompt: "read workspace".into(),
        mode: OperatingMode::Auto,
    });
    assert_eq!(auto.code, ExitCode::Success);
    assert!(read_file(&source, 10).expect("read").contains("1: alpha"));
    assert_eq!(search_literal(&root, "needle").expect("search").len(), 1);

    let plan_result = run_fake_headless(HeadlessRequest {
        prompt: "plan workspace".into(),
        mode: OperatingMode::Plan,
    });
    assert_eq!(plan_result.code, ExitCode::ApprovalRequired);

    let stale = write_file(
        &source,
        "wrong\n",
        Some(FilePrecondition::ExactText("stale\n".into())),
    );
    assert!(stale.is_err());
    write_file(
        &source,
        "alpha\nneedle changed\n",
        Some(FilePrecondition::ExactText("alpha\nneedle\n".into())),
    )
    .expect("write");
    apply_exact_patch(&source, "needle changed", "needle final").expect("patch");

    let compacted = compact(
        &[
            ContextItem::Text("transcript".into()),
            ContextItem::Todo("todo".into()),
            ContextItem::Plan("plan".into()),
            ContextItem::Goal("goal".into()),
            ContextItem::ToolPair("tool".into()),
        ],
        "summary",
    );
    assert_eq!(compacted.preserved.len(), 4);
    let artifacts = ArtifactStore::new(root.join("artifacts")).expect("artifacts");
    let handle = artifacts.put("output", b"full output").expect("artifact");
    assert_eq!(
        artifacts.read(&handle).expect("artifact read"),
        b"full output"
    );

    let skill_root = root.join("skills");
    let skill_dir = skill_root.join("demo");
    fs::create_dir_all(&skill_dir).expect("skill");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: demo skill\n---\nbody\n",
    )
    .expect("skill body");
    let skills = discover(&[SkillRoot::new(&skill_root, 0)]).expect("skills");
    assert_eq!(skills.active("demo").expect("skill").metadata.name, "demo");
    assert_eq!(
        read_body(skill_dir.join("SKILL.md")).expect("body"),
        "body\n"
    );

    let provider = FakeProvider::success();
    assert_eq!(provider.model(), "fake-model");
    let mut mcp = McpCatalog::new("demo");
    mcp.add_tool("read");
    mcp.add_resource("file://one");
    mcp.add_prompt("summarize");
    mcp.select_resource("file://one").expect("resource");
    mcp.select_prompt("summarize").expect("prompt");
    assert_eq!(
        mcp.tools_for_mode(OperatingMode::Auto),
        vec!["mcp.demo.read"]
    );
    assert_eq!(
        mcp.call_tool(OperatingMode::Auto, "read", "{}")
            .expect("mcp tool"),
        "mcp.demo.read({})"
    );

    let mut scheduler = Scheduler::new(4, 32);
    assert_eq!(
        scheduler.spawn(SpawnRequest::new("child", 1, true)),
        SpawnResult::Started
    );
    scheduler.finish("child", "done");
    assert_eq!(scheduler.status("child"), Some(ChildStatus::Completed));
    assert_eq!(scheduler.list().len(), 1);

    let mut queue = PromptQueue::new(8);
    queue.push("first").expect("queue");
    queue.push("second").expect("queue");
    assert_eq!(queue.pop().as_deref(), Some("first"));
    assert_eq!(queue.pop().as_deref(), Some("second"));

    let input_required = run_fake_headless(HeadlessRequest {
        prompt: String::new(),
        mode: OperatingMode::Auto,
    });
    assert_eq!(input_required.code, ExitCode::InputRequired);
    let resumed = run_fake_headless(HeadlessRequest {
        prompt: "answer".into(),
        mode: OperatingMode::Auto,
    });
    assert_eq!(resumed.code, ExitCode::Success);

    let session_path = root.join("session.jsonl");
    let mut writer = SessionWriter::create(&session_path, "e2e", "D:\\Slim").expect("session");
    writer
        .append(&SessionEvent::new(
            1,
            EventKind::SessionStarted {
                session_id: "e2e".into(),
            },
        ))
        .expect("event");
    writer
        .append(&SessionEvent::new(
            2,
            EventKind::AssistantTextDelta { text: "ok".into() },
        ))
        .expect("event");
    drop(writer);
    assert_eq!(recover(&session_path).expect("replay").events.len(), 2);
    let child_session = branch(&session_path, "child", 1).expect("branch");
    assert_eq!(
        recover(child_session).expect("child replay").events.len(),
        1
    );

    let mut todo = TodoTracker::new();
    let todo_id = todo.add("read");
    todo.set_status(todo_id, TodoStatus::Completed)
        .expect("todo");
    let mut plan = Plan::new();
    plan.add_node("read", &[]).expect("plan");
    assert_eq!(plan.approve().expect("approve"), 1);
    let mut goal = Goal::new(Some(100));
    goal.consume(1).expect("budget");
    goal.complete(Assurance::Verified).expect("goal");

    let mut handle = AppHandle::fake();
    handle
        .push_event(SessionEvent::new(
            3,
            EventKind::AssistantTextDelta { text: "ok".into() },
        ))
        .expect("event");
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::SessionSnapshot {
            session_id: SessionId(Arc::from("e2e")),
            cwd: "D:\\Slim".into(),
            skill_names: Vec::new(),
        }),
    );
    for event in handle.drain_events() {
        if let Some(ui_event) = UiEvent::from_core(event) {
            reduce(&mut state, Action::UiEventReceived(ui_event));
        }
    }
    let frame = ViewModel::derive(&state);
    assert_eq!(frame.lines[0], "Slim");
    assert_eq!(frame.lines[1], "ok");

    fs::remove_dir_all(root).expect("cleanup");
}
