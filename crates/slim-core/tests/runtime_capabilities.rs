use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use slim_core::mcp::McpCatalog;
use slim_core::runtime::RuntimeCapabilityBridge;
use slim_core::session::{
    AuthorizationGrant, CapabilityCatalog, DurableSessionHeader, JsonlRepo, MemoryRepo,
    TaskGoalAssurance, TaskMutation, TaskMutationRequest, TaskTodoStatus,
};
use slim_core::OperatingMode;

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
fn targeted_todos_reopen_with_initial_status_and_legacy_records() {
    let repo = MemoryRepo::new(DurableSessionHeader::new(
        "todos",
        "now",
        "workspace",
        None,
        None,
    ));
    let mut bridge =
        RuntimeCapabilityBridge::new(repo, CapabilityCatalog::with_native_tools()).unwrap();
    let legacy_add = serde_json::json!({"TodoAdd":{"title":"first"}});
    let mutations = [
        serde_json::from_value(legacy_add.clone()).unwrap(),
        TaskMutation::TodoAdd {
            title: "second".into(),
            status: None,
        },
        serde_json::from_value(serde_json::json!({"TodoSetStatus":{"status":"in_progress"}}))
            .unwrap(),
        TaskMutation::TodoAdd {
            title: "third".into(),
            status: None,
        },
        TaskMutation::TodoSetStatus {
            reason: None,
            id: Some(0),
            status: TaskTodoStatus::Completed,
        },
        TaskMutation::TodoSetStatus {
            reason: None,
            id: Some(2),
            status: TaskTodoStatus::InProgress,
        },
        TaskMutation::TodoSetStatus {
            reason: Some("dependency unavailable".into()),
            id: Some(1),
            status: TaskTodoStatus::Blocked,
        },
        TaskMutation::TodoAdd {
            title: "already done".into(),
            status: Some(TaskTodoStatus::Completed),
        },
    ];
    assert_eq!(serde_json::to_value(&mutations[0]).unwrap(), legacy_add);
    for (index, mutation) in mutations.into_iter().enumerate() {
        bridge
            .apply_task_mutation(
                task_request(
                    &format!("todo-{index}"),
                    "session",
                    index as u64 + 1,
                    mutation,
                ),
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            )
            .unwrap();
    }
    let before = bridge.todo("session").unwrap().items().to_vec();
    assert_eq!(
        before.iter().map(|item| item.status).collect::<Vec<_>>(),
        vec![
            slim_core::task::TodoStatus::Completed,
            slim_core::task::TodoStatus::Blocked,
            slim_core::task::TodoStatus::InProgress,
            slim_core::task::TodoStatus::Completed,
        ]
    );
    assert_eq!(before[1].reason.as_deref(), Some("dependency unavailable"));
    let repo = bridge.into_service().into_repo();
    let restored =
        RuntimeCapabilityBridge::new(repo, CapabilityCatalog::with_native_tools()).unwrap();
    assert_eq!(restored.todo("session").unwrap().items(), before);
}

#[test]
fn task_models_authorize_apply_and_reopen_without_replay() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("slim-runtime-tasks-{stamp}"));
    fs::create_dir_all(&root).expect("root");
    let session_root = root.join("session.jsonl");
    let repo = JsonlRepo::create(
        &session_root,
        DurableSessionHeader::new("runtime", "now", "D:\\Slim", None, None),
    )
    .expect("jsonl");
    let mut bridge =
        RuntimeCapabilityBridge::new(repo, CapabilityCatalog::with_native_tools()).expect("bridge");
    for (key, entity, revision, mutation) in [
        (
            "todo-add",
            "todo",
            1,
            TaskMutation::TodoAdd {
                status: None,
                title: "offline todo".into(),
            },
        ),
        (
            "todo-start",
            "todo",
            2,
            TaskMutation::TodoSetStatus {
                reason: None,
                id: None,
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
            status: None,
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

    drop(bridge.into_service().into_repo());
    let reopened = RuntimeCapabilityBridge::new(
        JsonlRepo::open(&session_root).expect("reopen jsonl"),
        CapabilityCatalog::with_native_tools(),
    )
    .expect("reopen bridge");
    assert_eq!(reopened.service().task_revision("plan"), 2);
    assert_eq!(reopened.todo("todo").expect("todo").items().len(), 1);
    assert!(reopened.plan("plan").expect("plan").is_approved());
    assert_eq!(
        reopened.goal("goal").expect("goal").assurance(),
        Some(slim_core::task::Assurance::Verified)
    );
    drop(reopened);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn runtime_catalog_preserves_native_modes_and_mcp_selection_boundary() {
    let mut unselected = McpCatalog::new("offline");
    unselected.add_resource("resource");
    let mut catalog = CapabilityCatalog::with_native_tools();
    catalog.add_mcp_catalog(&unselected).expect("catalog");
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
