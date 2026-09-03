use std::cell::Cell;
use std::fs;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use slim_core::mcp::McpCatalog;
use slim_core::runtime::{Runtime, RuntimeCapabilityAdapter, RuntimeCapabilityTarget};
use slim_core::session::{
    AuthorizationGrant, AuthorizationRequirement, CapabilityCatalog, CapabilityDescriptor,
    CapabilityKind, CapabilityRequest, CapabilitySelection, CapabilityTerminal, ChildRequest,
    DurableChildStatus, DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo, MemoryRepo,
    ReplayPolicy, TaskGoalAssurance, TaskMutation, TaskMutationRequest, TaskTodoStatus,
};
use slim_core::skills::{discover, SkillRoot};
use slim_core::OperatingMode;

struct RecordingAdapter {
    skill_calls: Cell<usize>,
    mcp_calls: Cell<usize>,
    effects: Vec<String>,
}

struct RetryAdapter {
    requests: Vec<(String, OperatingMode, AuthorizationGrant)>,
}

impl RuntimeCapabilityAdapter for RetryAdapter {
    fn dispatch(
        &mut self,
        _effect_id: &str,
        _descriptor: &CapabilityDescriptor,
        _target: &RuntimeCapabilityTarget,
        request: &CapabilityRequest,
    ) -> CapabilityTerminal {
        self.requests.push((
            request.execution_id.clone(),
            request.mode,
            request.authorization,
        ));
        CapabilityTerminal::Success
    }
}

struct FaultRepo {
    inner: MemoryRepo,
    fail_terminal_once: bool,
}

impl DurableRepo for FaultRepo {
    fn header(&self) -> &DurableSessionHeader {
        self.inner.header()
    }

    fn records(&self) -> &[DurableRecord] {
        self.inner.records()
    }

    fn append(&mut self, record: DurableRecord) -> io::Result<()> {
        if self.fail_terminal_once
            && matches!(&record, DurableRecord::Fact { fact, .. } if fact.key.contains(".terminal"))
        {
            self.fail_terminal_once = false;
            return Err(io::Error::other("injected terminal append failure"));
        }
        self.inner.append(record)
    }
}

impl RuntimeCapabilityAdapter for RecordingAdapter {
    fn dispatch(
        &mut self,
        effect_id: &str,
        descriptor: &CapabilityDescriptor,
        target: &RuntimeCapabilityTarget,
        _request: &CapabilityRequest,
    ) -> CapabilityTerminal {
        self.effects.push(effect_id.into());
        match (descriptor.kind, target) {
            (CapabilityKind::Skill, RuntimeCapabilityTarget::Skill { .. }) => {
                self.skill_calls.set(self.skill_calls.get() + 1);
                CapabilityTerminal::Success
            }
            (CapabilityKind::McpResource, RuntimeCapabilityTarget::McpResource { .. }) => {
                self.mcp_calls.set(self.mcp_calls.get() + 1);
                CapabilityTerminal::Success
            }
            _ => CapabilityTerminal::Failed,
        }
    }
}

fn fixture_root() -> (std::path::PathBuf, slim_core::skills::DiscoveryResult) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("slim-runtime-capabilities-{stamp}"));
    let skill = root.join("review");
    fs::create_dir_all(&skill).expect("skill dir");
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: offline fixture\n---\nfixture\n",
    )
    .expect("skill metadata");
    let discovery = discover(&[SkillRoot::new(&root, 0)]).expect("discover");
    (root, discovery)
}

fn mcp_fixture() -> McpCatalog {
    let mut mcp = McpCatalog::new("offline");
    mcp.add_resource("selected");
    mcp.select_resource("selected").expect("select resource");
    mcp
}

fn task_request(
    idempotency_key: &str,
    entity_id: &str,
    revision: u64,
    mutation: TaskMutation,
) -> TaskMutationRequest {
    TaskMutationRequest {
        idempotency_key: idempotency_key.into(),
        entity_id: entity_id.into(),
        revision,
        mutation,
    }
}

#[test]
fn runtime_bridge_runs_skill_selected_mcp_child_tasks_and_reopens_without_replay() {
    let (root, discovery) = fixture_root();
    let mcp = mcp_fixture();
    let session_root = root.join("session.jsonl");
    let repo = JsonlRepo::create(
        &session_root,
        DurableSessionHeader::new("runtime", "now", "D:\\Slim", None, None),
    )
    .expect("jsonl");
    let runtime = Runtime::new();
    let mut bridge = runtime
        .open_capability_bridge(repo, &discovery, std::slice::from_ref(&mcp))
        .expect("bridge");
    let mut adapter = RecordingAdapter {
        skill_calls: Cell::new(0),
        mcp_calls: Cell::new(0),
        effects: Vec::new(),
    };

    runtime
        .dispatch_capability(
            &mut bridge,
            CapabilityRequest::new(
                "skill-q",
                "skill-exec",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            &mut adapter,
        )
        .expect("skill");
    bridge
        .dispatch(
            CapabilityRequest::new(
                "mcp-q",
                "mcp-exec",
                "mcp.offline.resource.selected",
                OperatingMode::ReadOnly,
                AuthorizationGrant::None,
            ),
            &mut adapter,
        )
        .expect("selected mcp");
    assert_eq!(adapter.skill_calls.get(), 1);
    assert_eq!(adapter.mcp_calls.get(), 1);
    assert_eq!(
        adapter.effects,
        ["effect.runtime.skill-exec", "effect.runtime.mcp-exec"]
    );

    bridge
        .service_mut()
        .enqueue(CapabilityRequest::new(
            "root-q",
            "root-exec",
            "skill.review",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        ))
        .expect("active parent intent");

    bridge
        .enqueue_child(
            ChildRequest::new(
                "child-q",
                "child-exec",
                "root-exec",
                "runtime",
                "child-session",
            ),
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("child");
    assert_eq!(bridge.scheduler_depth("child-q"), Some(1));
    bridge
        .finish_child(
            "child-q",
            DurableChildStatus::Completed,
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("child terminal");
    for index in 0..4 {
        bridge
            .enqueue_child(
                ChildRequest::new(
                    format!("active-{index}"),
                    format!("active-exec-{index}"),
                    "root-exec",
                    "runtime",
                    format!("active-session-{index}"),
                ),
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            )
            .expect("active child");
    }
    for (queue_id, execution_id) in [("z-queued", "z-exec"), ("a-queued", "a-exec")] {
        bridge
            .enqueue_child(
                ChildRequest::new(
                    queue_id,
                    execution_id,
                    "root-exec",
                    "runtime",
                    format!("{queue_id}-session"),
                ),
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            )
            .expect("queued child");
    }
    assert_eq!(
        bridge.scheduler_queue_ids(),
        vec!["z-queued".to_owned(), "a-queued".to_owned()]
    );

    runtime
        .enqueue_capability(
            &mut bridge,
            CapabilityRequest::new(
                "queued-cap",
                "queued-execution",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
        )
        .expect("queued capability");

    for (key, entity, revision, mutation) in [
        (
            "todo-add",
            "todo",
            1,
            TaskMutation::TodoAdd {
                title: "offline todo".into(),
            },
        ),
        (
            "todo-start",
            "todo",
            2,
            TaskMutation::TodoSetStatus {
                status: TaskTodoStatus::InProgress,
            },
        ),
        (
            "plan-node",
            "plan",
            1,
            TaskMutation::PlanAddNode {
                node_id: "root".into(),
                dependencies: Vec::new(),
            },
        ),
        ("plan-approve", "plan", 2, TaskMutation::PlanApprove),
        (
            "goal-budget",
            "goal",
            1,
            TaskMutation::GoalSetBudget { budget: Some(1) },
        ),
        (
            "goal-consume",
            "goal",
            2,
            TaskMutation::GoalConsume { amount: 1 },
        ),
        (
            "goal-complete",
            "goal",
            3,
            TaskMutation::GoalComplete {
                assurance: TaskGoalAssurance::Verified,
            },
        ),
    ] {
        assert!(bridge
            .apply_task_mutation(
                task_request(key, entity, revision, mutation),
                OperatingMode::Plan,
                AuthorizationGrant::Explicit,
            )
            .expect("task mutation"));
    }
    let duplicate_todo = task_request(
        "todo-add",
        "todo",
        1,
        TaskMutation::TodoAdd {
            title: "offline todo".into(),
        },
    );
    assert!(bridge
        .apply_task_mutation(
            duplicate_todo.clone(),
            OperatingMode::ReadOnly,
            AuthorizationGrant::None,
        )
        .is_err());
    assert!(!bridge
        .apply_task_mutation(
            duplicate_todo,
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        )
        .expect("authorized duplicate is idempotent"));
    assert_eq!(bridge.todo("todo").expect("todo").items().len(), 1);
    assert!(bridge.plan("plan").expect("plan").is_approved());
    assert_eq!(
        bridge.goal("goal").expect("goal").assurance(),
        Some(slim_core::task::Assurance::Verified)
    );

    let repo = bridge.into_service().into_repo();
    drop(repo);
    let runtime = Runtime::new();
    let reopened_repo = JsonlRepo::open(&session_root).expect("reopen jsonl");
    let mut reopened = runtime
        .open_capability_bridge(reopened_repo, &discovery, &[mcp])
        .expect("reopen bridge");
    let mut replay_adapter = RecordingAdapter {
        skill_calls: Cell::new(0),
        mcp_calls: Cell::new(0),
        effects: Vec::new(),
    };
    assert!(reopened
        .dispatch(
            CapabilityRequest::new(
                "skill-q",
                "skill-exec",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            &mut replay_adapter,
        )
        .is_err());
    assert!(replay_adapter.effects.is_empty());
    assert_eq!(
        reopened.child_status("child-q"),
        Some(DurableChildStatus::Completed)
    );
    assert_eq!(
        reopened.scheduler_status("child-q"),
        Some(slim_core::agents::ChildStatus::Completed)
    );
    assert_eq!(reopened.scheduler_depth("child-q"), Some(1));
    assert!(reopened
        .child_token("child-q")
        .expect("child token")
        .is_cancelled());
    assert_eq!(reopened.service().task_revision("plan"), 2);
    assert_eq!(
        reopened.scheduler_queue_ids(),
        vec!["z-queued".to_owned(), "a-queued".to_owned()]
    );
    runtime
        .dispatch_queued_capability(&mut reopened, "queued-cap", &mut replay_adapter)
        .expect("queued capability dispatch");
    assert_eq!(replay_adapter.skill_calls.get(), 1);
    drop(reopened);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn runtime_bridge_child_cancel_propagates_scheduler_token() {
    let (root, discovery) = fixture_root();
    let repo = JsonlRepo::create(
        root.join("session.jsonl"),
        DurableSessionHeader::new("runtime", "now", "D:\\Slim", None, None),
    )
    .expect("jsonl");
    let runtime = Runtime::new();
    let mut bridge = runtime
        .open_capability_bridge(repo, &discovery, &[])
        .expect("bridge");
    bridge
        .service_mut()
        .enqueue(slim_core::session::CapabilityRequest::new(
            "parent-q",
            "parent-exec",
            "tool.read",
            OperatingMode::ReadOnly,
            AuthorizationGrant::None,
        ))
        .expect("parent");
    bridge
        .enqueue_child(
            ChildRequest::new(
                "child-q",
                "child-exec",
                "parent-exec",
                "runtime",
                "child-session",
            ),
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("child");
    bridge
        .cancel_child("child-q", OperatingMode::Auto, AuthorizationGrant::Explicit)
        .expect("cancel request");
    assert!(bridge.child_token("child-q").expect("token").is_cancelled());
    assert_eq!(
        bridge.child_status("child-q"),
        Some(DurableChildStatus::CancellationRequested)
    );
    assert_eq!(
        bridge.scheduler_status("child-q"),
        Some(slim_core::agents::ChildStatus::Active)
    );
    bridge
        .finish_child(
            "child-q",
            DurableChildStatus::Cancelled,
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("cancel terminal");
    assert_eq!(
        bridge.scheduler_status("child-q"),
        Some(slim_core::agents::ChildStatus::Cancelled)
    );
    runtime
        .enqueue_capability(
            &mut bridge,
            CapabilityRequest::new(
                "cap-q",
                "cap-exec",
                "tool.read",
                OperatingMode::ReadOnly,
                AuthorizationGrant::None,
            ),
        )
        .expect("capability intent");
    runtime
        .cancel_capability(
            &mut bridge,
            "cap-q",
            OperatingMode::ReadOnly,
            AuthorizationGrant::None,
        )
        .expect("capability cancel");
    assert!(bridge
        .capability_token("cap-q")
        .expect("token")
        .is_cancelled());
    assert_eq!(
        bridge.service().capability_status("cap-q"),
        Some(CapabilityTerminal::Cancelled)
    );
    drop(bridge);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn runtime_catalog_preserves_native_modes_and_mcp_selection_boundary() {
    let (_root, discovery) = fixture_root();
    let mut unselected = McpCatalog::new("offline");
    unselected.add_resource("resource");
    let runtime = Runtime::new();
    let catalog = runtime
        .capability_catalog(&discovery, &[unselected])
        .expect("catalog");
    assert!(catalog
        .descriptor("tool.read")
        .expect("read")
        .allows_mode(OperatingMode::ReadOnly));
    assert!(!catalog
        .descriptor("tool.write")
        .expect("write")
        .allows_mode(OperatingMode::Plan));
}

#[test]
fn cancellation_token_broadcasts_to_all_waiters() {
    let token = slim_core::runtime::CancellationToken::new();
    let runtime = tokio::runtime::Runtime::new().expect("tokio");
    runtime.block_on(async {
        let first = {
            let token = token.clone();
            tokio::spawn(async move { token.cancelled().await })
        };
        let second = {
            let token = token.clone();
            tokio::spawn(async move { token.cancelled().await })
        };
        token.cancel();
        let (first, second) = tokio::join!(first, second);
        first.expect("first waiter");
        second.expect("second waiter");
    });
}

#[test]
fn retry_passes_the_original_durable_request_to_adapter() {
    let (root, discovery) = fixture_root();
    let repo = FaultRepo {
        inner: MemoryRepo::new(DurableSessionHeader::new(
            "runtime", "now", "D:\\Slim", None, None,
        )),
        fail_terminal_once: true,
    };
    let mut catalog = CapabilityCatalog::new();
    catalog
        .add(CapabilityDescriptor {
            id: "tool.read".into(),
            kind: CapabilityKind::NativeTool,
            allowed_modes: vec![OperatingMode::ReadOnly],
            authorization: AuthorizationRequirement::None,
            replay_policy: ReplayPolicy::Safe,
            mutates_workspace: false,
            selection: CapabilitySelection::None,
        })
        .expect("safe descriptor");
    let mut bridge = slim_core::runtime::RuntimeCapabilityBridge::new(
        repo,
        catalog,
        &discovery,
        &[],
        slim_core::tools::ToolRegistry::default(),
        slim_core::runtime::CancellationToken::new(),
    )
    .expect("bridge");
    let mut adapter = RetryAdapter {
        requests: Vec::new(),
    };
    assert!(bridge
        .dispatch(
            CapabilityRequest::new(
                "retry-q",
                "original-execution",
                "tool.read",
                OperatingMode::ReadOnly,
                AuthorizationGrant::None,
            ),
            &mut adapter,
        )
        .is_err());
    assert_eq!(
        adapter.requests,
        vec![(
            "original-execution".into(),
            OperatingMode::ReadOnly,
            AuthorizationGrant::None
        )]
    );
    bridge
        .retry_in_flight("retry-q", &mut adapter)
        .expect("explicit retry");
    assert_eq!(adapter.requests.len(), 2);
    assert_eq!(adapter.requests[1], adapter.requests[0]);
    drop(bridge);
    fs::remove_dir_all(root).expect("cleanup");
}
