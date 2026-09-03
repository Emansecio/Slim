use std::cell::Cell;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use slim_core::mcp::McpCatalog;
use slim_core::session::{
    AuthorizationGrant, AuthorizationRequirement, CapabilityCatalog, CapabilityDescriptor,
    CapabilityDispatcher, CapabilityKind, CapabilityLedgerError, CapabilityRequest,
    CapabilitySelection, CapabilityService, CapabilityTerminal, ChildRequest, DurableChildStatus,
    DurableFact, DurableRecord, DurableRepo, DurableSessionHeader, JsonlRepo, MemoryRepo,
    ReplayPolicy, TaskMutation, TaskMutationRequest, TaskTodoStatus, MAX_ACTIVE_CHILDREN,
    MAX_BASE_ID_BYTES, MAX_CAPABILITY_ID_BYTES, MAX_CAPABILITY_QUEUE, MAX_CHILD_QUEUE,
    MAX_FACT_BYTES,
};
use slim_core::OperatingMode;
use std::io;

struct FaultRepo {
    inner: MemoryRepo,
    fail_terminal_once: bool,
    fail_child_started_once: bool,
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
            && matches!(
                &record,
                DurableRecord::Fact { fact, .. } if fact.key.contains("terminal")
            )
        {
            self.fail_terminal_once = false;
            return Err(io::Error::other("injected terminal append failure"));
        }
        if self.fail_child_started_once
            && matches!(
                &record,
                DurableRecord::Fact { fact, .. } if fact.key.contains("child_started")
            )
        {
            self.fail_child_started_once = false;
            return Err(io::Error::other("injected child promotion append failure"));
        }
        self.inner.append(record)
    }
}

fn service() -> CapabilityService<MemoryRepo> {
    CapabilityService::new(
        MemoryRepo::new(DurableSessionHeader::new(
            "capability-test",
            "now",
            "D:\\Slim",
            None,
            None,
        )),
        catalog(),
    )
    .expect("service")
}

fn catalog() -> CapabilityCatalog {
    let mut catalog = CapabilityCatalog::with_native_tools();
    catalog.add_skill("review").expect("skill");
    let mut mcp = McpCatalog::new("fixture");
    mcp.add_tool("fake");
    mcp.add_resource("resource://offline");
    mcp.add_prompt("summarize");
    catalog.add_mcp_catalog(&mcp).expect("mcp");
    catalog
}

#[test]
fn catalog_composes_native_skill_and_mcp_policy_without_transport() {
    let mut catalog = CapabilityCatalog::with_native_tools();
    catalog.add_skill("review").expect("skill");
    let mut mcp = McpCatalog::new("fixture");
    mcp.add_tool("fake");
    mcp.add_resource("resource://offline");
    catalog.add_mcp_catalog(&mcp).expect("mcp");

    assert!(catalog
        .descriptor("tool.read")
        .expect("read")
        .allows_mode(OperatingMode::ReadOnly));
    assert!(!catalog
        .descriptor("tool.write")
        .expect("write")
        .allows_mode(OperatingMode::Plan));
    assert!(catalog
        .authorize(
            "skill.review",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit
        )
        .is_ok());
    assert!(matches!(
        catalog.authorize(
            "mcp.fixture.fake",
            OperatingMode::Auto,
            AuthorizationGrant::Trusted
        ),
        Err(CapabilityLedgerError::AuthorizationRequired { .. })
    ));
    assert!(matches!(
        catalog.authorize(
            "mcp.fixture.resource.resource://offline",
            OperatingMode::ReadOnly,
            AuthorizationGrant::None
        ),
        Err(CapabilityLedgerError::SelectionRequired { .. })
    ));
}

#[test]
fn intent_is_persisted_before_offline_dispatch_and_restore_does_not_replay() {
    let mut active_service = service();
    let calls = Cell::new(0);
    let outcome = active_service
        .dispatch(
            CapabilityRequest::new(
                "q-skill",
                "exec-skill",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            |_, effect_id| {
                calls.set(calls.get() + 1);
                assert_eq!(effect_id, "effect.capability-test.exec-skill");
                CapabilityTerminal::Success
            },
        )
        .expect("dispatch");
    assert_eq!(outcome.status, CapabilityTerminal::Success);
    assert_eq!(calls.get(), 1);
    let bytes = serde_json::to_string(active_service.repo().records()).expect("records");
    assert!(!bytes.contains("secret-prompt"));
    assert!(bytes.contains("q-skill"));

    let repo = active_service.into_repo();
    let restored = CapabilityService::new(repo, CapabilityCatalog::with_native_tools());
    assert!(restored.is_err(), "catalog mismatch must fail closed");

    // Rebuild the same catalog and verify a terminal record is not dispatched
    // again after reopen.
    let mut reopened = service();
    let _ = reopened
        .dispatch(
            CapabilityRequest::new(
                "q-skill",
                "exec-skill",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            |_, _| CapabilityTerminal::Success,
        )
        .expect("seed dispatch");
    let repo = reopened.into_repo();
    let mut restored = service_from_repo(repo);
    let calls = Cell::new(0);
    assert!(matches!(
        restored.dispatch_queued("q-skill", |_, _| {
            calls.set(calls.get() + 1);
            CapabilityTerminal::Success
        }),
        Err(CapabilityLedgerError::AlreadyTerminal(_))
    ));
    assert_eq!(calls.get(), 0);
}

fn service_from_repo(repo: MemoryRepo) -> CapabilityService<MemoryRepo> {
    CapabilityService::new(repo, catalog()).expect("reopen")
}

fn seed_root(service: &mut CapabilityService<MemoryRepo>) {
    service
        .enqueue(CapabilityRequest::new(
            "root-q",
            "root-exec",
            "skill.review",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        ))
        .expect("root");
}

#[test]
fn jsonl_e2e_skill_mcp_child_task_terminal_reopens_without_replay() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("slim-capabilities-{stamp}"));
    fs::create_dir(&root).expect("temp root");
    let path = root.join("session.jsonl");
    let mut service = CapabilityService::new(
        JsonlRepo::create(
            &path,
            DurableSessionHeader::new("jsonl", "now", "D:\\Slim", None, None),
        )
        .expect("jsonl create"),
        catalog(),
    )
    .expect("service");
    let dispatch_count = Cell::new(0);
    service
        .enqueue(CapabilityRequest::new(
            "root-q",
            "root-exec",
            "skill.review",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        ))
        .expect("live root capability");
    service
        .dispatch(
            CapabilityRequest::new(
                "skill-q",
                "skill-exec",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            |_, _| {
                dispatch_count.set(dispatch_count.get() + 1);
                CapabilityTerminal::Success
            },
        )
        .expect("skill dispatch");
    service
        .enqueue_child(
            ChildRequest::new(
                "child-q",
                "child-exec",
                "root-exec",
                "jsonl",
                "child-session",
            ),
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("child");
    service
        .finish_child(
            "child-q",
            DurableChildStatus::Completed,
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("child terminal");
    assert!(service
        .apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "plan-1".into(),
                entity_id: "plan".into(),
                revision: 1,
                mutation: TaskMutation::PlanAddNode {
                    node_id: "root".into(),
                    dependencies: Vec::new(),
                },
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit
        )
        .expect("task"));
    assert!(service
        .apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "plan-2".into(),
                entity_id: "plan".into(),
                revision: 2,
                mutation: TaskMutation::PlanApprove,
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        )
        .expect("plan approval"));
    assert_eq!(dispatch_count.get(), 1);
    let repo = service.into_repo();
    drop(repo);

    let mut reopened =
        CapabilityService::new(JsonlRepo::open(&path).expect("jsonl reopen"), catalog())
            .expect("restore");
    assert_eq!(
        reopened.capability_status("skill-q"),
        Some(CapabilityTerminal::Success)
    );
    assert_eq!(
        reopened.child_status("child-q"),
        Some(DurableChildStatus::Completed)
    );
    assert_eq!(reopened.task_revision("plan"), 2);
    assert!(matches!(
        reopened.dispatch_queued("skill-q", |_, _| {
            panic!("restore must not dispatch terminal capability")
        }),
        Err(CapabilityLedgerError::AlreadyTerminal(_))
    ));
    drop(reopened);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn child_state_is_bounded_fifo_and_restore_has_no_implicit_execution() {
    let mut service = service();
    seed_root(&mut service);
    for index in 0..MAX_ACTIVE_CHILDREN {
        service
            .enqueue_child(
                ChildRequest::new(
                    format!("child-{index}"),
                    format!("exec-{index}"),
                    "root-exec",
                    "capability-test",
                    format!("session-{index}"),
                ),
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            )
            .expect("active child");
        assert_eq!(
            service.child_status(&format!("child-{index}")),
            Some(DurableChildStatus::Active)
        );
    }
    for index in 0..MAX_CHILD_QUEUE {
        service
            .enqueue_child(
                ChildRequest::new(
                    format!("queued-{index}"),
                    format!("queued-exec-{index}"),
                    "root-exec",
                    "capability-test",
                    format!("queued-session-{index}"),
                ),
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            )
            .expect("queued child");
    }
    assert_eq!(service.child_queue().len(), MAX_CHILD_QUEUE);
    assert!(matches!(
        service.enqueue_child(
            ChildRequest::new(
                "overflow",
                "overflow-exec",
                "root-exec",
                "capability-test",
                "s",
            ),
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        ),
        Err(CapabilityLedgerError::ChildQueueFull)
    ));
    assert!(matches!(
        service.enqueue_child(
            ChildRequest::new("deep", "deep-exec", "exec-0", "session-0", "deep-session",),
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        ),
        Err(CapabilityLedgerError::ChildDepthExceeded)
    ));

    service
        .cancel_child("child-0", OperatingMode::Auto, AuthorizationGrant::Explicit)
        .expect("cancel request");
    service
        .finish_child(
            "child-0",
            DurableChildStatus::Cancelled,
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("cancel terminal");
    assert_eq!(
        service.child_status("queued-0"),
        Some(DurableChildStatus::Active)
    );
    let repo = service.into_repo();
    let restored = service_from_repo(repo);
    assert_eq!(
        restored.child_status("child-0"),
        Some(DurableChildStatus::Cancelled)
    );
    assert_eq!(
        restored.child_status("queued-0"),
        Some(DurableChildStatus::Active)
    );
}

#[test]
fn typed_task_mutations_are_revisioned_idempotent_and_restorable() {
    let mut service = service();
    let first = TaskMutationRequest {
        idempotency_key: "todo-add-1".into(),
        entity_id: "todo".into(),
        revision: 1,
        mutation: TaskMutation::TodoAdd {
            title: "offline task".into(),
        },
    };
    assert!(service
        .apply_task_mutation(
            first.clone(),
            OperatingMode::Plan,
            AuthorizationGrant::Explicit
        )
        .expect("add"));
    assert!(!service
        .apply_task_mutation(first, OperatingMode::Plan, AuthorizationGrant::Explicit)
        .expect("idempotent retry"));
    assert_eq!(service.task_revision("todo"), 1);
    assert!(matches!(
        service.apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "todo-add-2".into(),
                entity_id: "todo".into(),
                revision: 3,
                mutation: TaskMutation::TodoSetStatus {
                    status: TaskTodoStatus::Completed,
                },
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit
        ),
        Err(CapabilityLedgerError::RevisionConflict { .. })
    ));
    let repo = service.into_repo();
    let restored = service_from_repo(repo);
    assert_eq!(restored.task_revision("todo"), 1);
    assert_eq!(restored.task_mutations("todo").len(), 1);
}

struct RecordingAdapter {
    seen: Vec<(String, String)>,
}

impl CapabilityDispatcher for RecordingAdapter {
    fn dispatch(
        &mut self,
        effect_id: &str,
        descriptor: &slim_core::session::CapabilityDescriptor,
    ) -> CapabilityTerminal {
        self.seen.push((effect_id.into(), descriptor.id.clone()));
        CapabilityTerminal::Success
    }
}

#[test]
fn skill_and_mcp_fake_adapters_share_effect_id_boundary() {
    let mut service = service();
    let mut skill = RecordingAdapter { seen: Vec::new() };
    service
        .dispatch_with_adapter(
            CapabilityRequest::new(
                "skill-adapter-q",
                "shared-skill-effect",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            &mut skill,
        )
        .expect("skill adapter");
    assert_eq!(
        skill.seen,
        vec![(
            "effect.capability-test.shared-skill-effect".into(),
            "skill.review".into(),
        )]
    );

    let mut mcp = RecordingAdapter { seen: Vec::new() };
    service
        .dispatch_with_adapter(
            CapabilityRequest::new(
                "mcp-adapter-q",
                "shared-mcp-effect",
                "mcp.fixture.fake",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            &mut mcp,
        )
        .expect("mcp adapter");
    assert_eq!(
        mcp.seen,
        vec![(
            "effect.capability-test.shared-mcp-effect".into(),
            "mcp.fixture.fake".into(),
        )]
    );
}

#[test]
fn claim_and_terminal_failure_are_inflight_and_never_implicitly_replayed() {
    let header = DurableSessionHeader::new("fault", "now", "D:\\Slim", None, None);
    let mut service = CapabilityService::new(
        FaultRepo {
            inner: MemoryRepo::new(header),
            fail_terminal_once: true,
            fail_child_started_once: false,
        },
        catalog(),
    )
    .expect("service");
    let calls = Cell::new(0);
    let error = service
        .dispatch(
            CapabilityRequest::new(
                "fault-q",
                "fault-exec",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            |_, effect_id| {
                calls.set(calls.get() + 1);
                assert_eq!(effect_id, "effect.fault.fault-exec");
                CapabilityTerminal::Success
            },
        )
        .expect_err("terminal append fault");
    assert!(matches!(error, CapabilityLedgerError::Persist(_)));
    assert_eq!(calls.get(), 1);
    assert_eq!(
        service.capability_state("fault-q"),
        Some(slim_core::session::CapabilityExecutionState::InFlightRequiresDecision)
    );
    assert!(matches!(
        service.dispatch_queued("fault-q", |_, _| {
            calls.set(calls.get() + 1);
            CapabilityTerminal::Success
        }),
        Err(CapabilityLedgerError::InFlightRequiresDecision(_))
    ));
    assert_eq!(calls.get(), 1);

    let repo = service.into_repo();
    let mut restored = CapabilityService::new(repo, catalog()).expect("restore");
    assert_eq!(calls.get(), 1);
    assert!(matches!(
        restored.dispatch_queued("fault-q", |_, _| {
            calls.set(calls.get() + 1);
            CapabilityTerminal::Success
        }),
        Err(CapabilityLedgerError::InFlightRequiresDecision(_))
    ));
    assert_eq!(calls.get(), 1);
    assert!(matches!(
        restored.retry_in_flight("fault-q", |_, effect_id| {
            calls.set(calls.get() + 1);
            assert_eq!(effect_id, "effect.fault.fault-exec");
            CapabilityTerminal::Success
        }),
        Err(CapabilityLedgerError::RetryNotAllowed(_))
    ));
    assert_eq!(calls.get(), 1);
}

#[test]
fn child_and_task_mutations_require_policy_and_child_depth_is_derived() {
    let mut service = service();
    seed_root(&mut service);
    let child = ChildRequest::new(
        "auth-child",
        "auth-child-exec",
        "root-exec",
        "capability-test",
        "auth-session",
    );
    assert!(matches!(
        service.enqueue_child(child.clone(), OperatingMode::Auto, AuthorizationGrant::None),
        Err(CapabilityLedgerError::AuthorizationRequired { .. })
    ));
    let mut forged = child;
    forged.depth = MAX_CHILD_QUEUE as u8;
    assert!(matches!(
        service.enqueue_child(forged, OperatingMode::Auto, AuthorizationGrant::Explicit),
        Err(CapabilityLedgerError::ChildDepthExceeded)
    ));
    service
        .enqueue_child(
            ChildRequest::new(
                "auth-child",
                "auth-child-exec",
                "root-exec",
                "capability-test",
                "auth-session",
            ),
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("authorized child");
    assert!(matches!(
        service.apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "unauthorized-task".into(),
                entity_id: "todo".into(),
                revision: 1,
                mutation: TaskMutation::TodoAdd { title: "x".into() },
            },
            OperatingMode::Plan,
            AuthorizationGrant::None,
        ),
        Err(CapabilityLedgerError::AuthorizationRequired { .. })
    ));
    assert!(matches!(
        service.apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "oversized-task".into(),
                entity_id: "todo".into(),
                revision: 1,
                mutation: TaskMutation::TodoAdd {
                    title: "x".repeat(5 * 1024),
                },
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        ),
        Err(CapabilityLedgerError::InvalidIdentifier(_))
    ));

    let mut completed_parent = CapabilityService::new(
        MemoryRepo::new(DurableSessionHeader::new(
            "completed-parent-test",
            "now",
            "D:\\Slim",
            None,
            None,
        )),
        catalog(),
    )
    .expect("capability service");
    completed_parent
        .dispatch(
            CapabilityRequest::new(
                "completed-parent-q",
                "completed-parent-exec",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            |_, _| CapabilityTerminal::Success,
        )
        .expect("complete parent");
    assert!(matches!(
        completed_parent.enqueue_child(
            ChildRequest::new(
                "late-child",
                "late-child-exec",
                "completed-parent-exec",
                "completed-parent-test",
                "late-child-session",
            ),
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        ),
        Err(CapabilityLedgerError::ParentNotActive(_))
    ));
}

#[test]
fn catalog_rejects_untrusted_duplicates_and_oversized_mcp_ids() {
    let mut catalog = CapabilityCatalog::new();
    catalog.add_skill("skill").expect("skill");
    assert!(matches!(
        catalog.add_skill("skill"),
        Err(CapabilityLedgerError::DuplicateCapability)
    ));
    assert!(matches!(
        catalog.add_skill("bad\nname"),
        Err(CapabilityLedgerError::InvalidIdentifier(_))
    ));
    assert!(matches!(
        catalog.authorize(
            "skill.skill",
            OperatingMode::Auto,
            AuthorizationGrant::Trusted
        ),
        Err(CapabilityLedgerError::AuthorizationRequired { .. })
    ));
    let mut mcp = McpCatalog::new("fixture");
    mcp.add_tool("x".repeat(MAX_FACT_BYTES));
    assert!(matches!(
        catalog.add_mcp_catalog(&mcp),
        Err(CapabilityLedgerError::InvalidIdentifier(_))
    ));
}

#[test]
fn capability_queue_bound_is_enforced_before_append() {
    let mut service = service();
    for index in 0..MAX_CAPABILITY_QUEUE {
        service
            .enqueue(CapabilityRequest::new(
                format!("queue-{index}"),
                format!("execution-{index}"),
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ))
            .expect("bounded queue item");
    }
    let before = service.repo().records().len();
    assert!(matches!(
        service.enqueue(CapabilityRequest::new(
            "queue-overflow",
            "execution-overflow",
            "skill.review",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )),
        Err(CapabilityLedgerError::QueueFull)
    ));
    assert_eq!(service.repo().records().len(), before);
}

#[test]
fn mcp_selection_is_catalog_derived_and_manual_descriptors_cannot_bypass_it() {
    let mut selected_mcp = McpCatalog::new("selected");
    selected_mcp.add_resource("resource://one");
    selected_mcp
        .select_resource("resource://one")
        .expect("select");
    let mut catalog = CapabilityCatalog::with_native_tools();
    catalog.add_mcp_catalog(&selected_mcp).expect("catalog");
    assert!(catalog
        .authorize(
            "mcp.selected.resource.resource://one",
            OperatingMode::ReadOnly,
            AuthorizationGrant::None,
        )
        .is_ok());

    let mut unselected_mcp = McpCatalog::new("unselected");
    unselected_mcp.add_resource("resource://two");
    catalog
        .add_mcp_catalog(&unselected_mcp)
        .expect("catalog unselected");
    assert!(matches!(
        catalog.authorize(
            "mcp.unselected.resource.resource://two",
            OperatingMode::ReadOnly,
            AuthorizationGrant::None,
        ),
        Err(CapabilityLedgerError::SelectionRequired { .. })
    ));
    assert!(matches!(
        catalog.add(CapabilityDescriptor {
            id: "manual.resource".into(),
            kind: CapabilityKind::McpResource,
            allowed_modes: vec![OperatingMode::ReadOnly],
            authorization: AuthorizationRequirement::None,
            replay_policy: ReplayPolicy::Never,
            mutates_workspace: false,
            selection: CapabilitySelection::None,
        }),
        Err(CapabilityLedgerError::InvalidCapabilityPolicy(_))
    ));
    assert!(matches!(
        catalog.add_skill("é".repeat(MAX_CAPABILITY_ID_BYTES)),
        Err(CapabilityLedgerError::InvalidIdentifier(_))
    ));
}

#[test]
fn task_fact_payload_is_bounded_before_append() {
    let mut service = service();
    let mut dependencies = Vec::new();
    for revision in 1..=6 {
        let node_id = format!("{revision}{}", "x".repeat(3_000));
        service
            .apply_task_mutation(
                TaskMutationRequest {
                    idempotency_key: format!("large-node-{revision}"),
                    entity_id: "large-plan".into(),
                    revision,
                    mutation: TaskMutation::PlanAddNode {
                        node_id: node_id.clone(),
                        dependencies: Vec::new(),
                    },
                },
                OperatingMode::Plan,
                AuthorizationGrant::Explicit,
            )
            .expect("bounded root node");
        dependencies.push(node_id);
    }

    assert!(matches!(
        service.apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "oversized-plan-fact".into(),
                entity_id: "large-plan".into(),
                revision: 7,
                mutation: TaskMutation::PlanAddNode {
                    node_id: "final".into(),
                    dependencies,
                },
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        ),
        Err(CapabilityLedgerError::InvalidIdentifier("fact"))
    ));
    assert_eq!(service.task_revision("large-plan"), 6);
}

#[test]
fn effect_ids_are_session_scoped_and_base_ids_reserve_derived_suffixes() {
    let mut first = service();
    let mut first_effect = String::new();
    first
        .dispatch(
            CapabilityRequest::new(
                "effect-q",
                "same-execution",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            |_, effect_id| {
                first_effect = effect_id.into();
                CapabilityTerminal::Success
            },
        )
        .expect("first dispatch");
    let mut second = CapabilityService::new(
        MemoryRepo::new(DurableSessionHeader::new(
            "other-session",
            "now",
            "D:\\Slim",
            None,
            None,
        )),
        catalog(),
    )
    .expect("second service");
    let mut second_effect = String::new();
    second
        .dispatch(
            CapabilityRequest::new(
                "effect-q",
                "same-execution",
                "skill.review",
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            ),
            |_, effect_id| {
                second_effect = effect_id.into();
                CapabilityTerminal::Success
            },
        )
        .expect("second dispatch");
    assert_ne!(first_effect, second_effect);

    let mut bounded = service();
    assert!(matches!(
        bounded.enqueue(CapabilityRequest::new(
            "q".repeat(MAX_BASE_ID_BYTES + 1),
            "short-execution",
            "skill.review",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )),
        Err(CapabilityLedgerError::InvalidIdentifier("base identifier"))
    ));
}

#[test]
fn queued_cancellation_is_terminal_and_never_promotes_or_resurrects() {
    let mut service = service();
    seed_root(&mut service);
    for index in 0..MAX_ACTIVE_CHILDREN {
        service
            .enqueue_child(
                ChildRequest::new(
                    format!("active-cancel-{index}"),
                    format!("active-cancel-exec-{index}"),
                    "root-exec",
                    "capability-test",
                    format!("active-cancel-session-{index}"),
                ),
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            )
            .expect("active child");
    }
    service
        .enqueue_child(
            ChildRequest::new(
                "queued-cancel",
                "queued-cancel-exec",
                "root-exec",
                "capability-test",
                "queued-cancel-session",
            ),
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("queued child");
    service
        .cancel_child(
            "queued-cancel",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("queued cancellation");
    assert_eq!(
        service.child_status("queued-cancel"),
        Some(DurableChildStatus::Cancelled)
    );
    assert!(service.child_queue().is_empty());
    let restored = service_from_repo(service.into_repo());
    assert_eq!(
        restored.child_status("queued-cancel"),
        Some(DurableChildStatus::Cancelled)
    );
}

#[test]
fn child_promotion_failure_is_recoverable_as_a_restore_plan() {
    let mut service = CapabilityService::new(
        FaultRepo {
            inner: MemoryRepo::new(DurableSessionHeader::new(
                "promotion-fault",
                "now",
                "D:\\Slim",
                None,
                None,
            )),
            fail_terminal_once: false,
            fail_child_started_once: true,
        },
        catalog(),
    )
    .expect("service");
    seed_root_for_fault(&mut service);
    for index in 0..MAX_ACTIVE_CHILDREN {
        service
            .enqueue_child(
                ChildRequest::new(
                    format!("promotion-active-{index}"),
                    format!("promotion-active-exec-{index}"),
                    "promotion-root-exec",
                    "promotion-fault",
                    format!("promotion-session-{index}"),
                ),
                OperatingMode::Auto,
                AuthorizationGrant::Explicit,
            )
            .expect("active child");
    }
    service
        .enqueue_child(
            ChildRequest::new(
                "promotion-queued",
                "promotion-queued-exec",
                "promotion-root-exec",
                "promotion-fault",
                "promotion-queued-session",
            ),
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("queued child");
    assert!(matches!(
        service.finish_child(
            "promotion-active-0",
            DurableChildStatus::Completed,
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        ),
        Err(CapabilityLedgerError::Persist(_))
    ));
    assert_eq!(
        service.child_status("promotion-active-0"),
        Some(DurableChildStatus::Completed)
    );
    assert_eq!(
        service.pending_child_promotions()[0].queue_id,
        "promotion-queued"
    );
    let mut restored = CapabilityService::new(service.into_repo(), catalog()).expect("restore");
    assert_eq!(
        restored.pending_child_promotions()[0].queue_id,
        "promotion-queued"
    );
    restored
        .promote_next_child(OperatingMode::Auto, AuthorizationGrant::Explicit)
        .expect("promotion claim")
        .expect("planned promotion");
    assert_eq!(
        restored.child_status("promotion-queued"),
        Some(DurableChildStatus::Active)
    );
}

fn seed_root_for_fault(service: &mut CapabilityService<FaultRepo>) {
    service
        .enqueue(CapabilityRequest::new(
            "promotion-root-q",
            "promotion-root-exec",
            "skill.review",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        ))
        .expect("root");
}

#[test]
fn task_projection_enforces_todo_plan_and_goal_invariants() {
    let mut service = service();
    let add_todo = |id: &str, entity: &str| TaskMutationRequest {
        idempotency_key: id.into(),
        entity_id: entity.into(),
        revision: 1,
        mutation: TaskMutation::TodoAdd {
            title: "todo".into(),
        },
    };
    service
        .apply_task_mutation(
            add_todo("todo-a-add", "todo-a"),
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        )
        .expect("todo a add");
    service
        .apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "todo-a-progress".into(),
                entity_id: "todo-a".into(),
                revision: 2,
                mutation: TaskMutation::TodoSetStatus {
                    status: TaskTodoStatus::InProgress,
                },
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        )
        .expect("todo a progress");
    service
        .apply_task_mutation(
            add_todo("todo-b-add", "todo-b"),
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        )
        .expect("todo b add");
    assert!(matches!(
        service.apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "todo-b-progress".into(),
                entity_id: "todo-b".into(),
                revision: 2,
                mutation: TaskMutation::TodoSetStatus {
                    status: TaskTodoStatus::InProgress,
                },
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        ),
        Err(CapabilityLedgerError::InvalidTaskTransition(_))
    ));

    assert!(matches!(
        service.apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "plan-missing-dep".into(),
                entity_id: "plan".into(),
                revision: 1,
                mutation: TaskMutation::PlanAddNode {
                    node_id: "child".into(),
                    dependencies: vec!["missing".into()],
                },
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        ),
        Err(CapabilityLedgerError::InvalidTaskTransition(_))
    ));
    for (key, revision, mutation) in [
        (
            "plan-root",
            1,
            TaskMutation::PlanAddNode {
                node_id: "root".into(),
                dependencies: Vec::new(),
            },
        ),
        (
            "plan-child",
            2,
            TaskMutation::PlanAddNode {
                node_id: "child".into(),
                dependencies: vec!["root".into()],
            },
        ),
        ("plan-approve", 3, TaskMutation::PlanApprove),
    ] {
        service
            .apply_task_mutation(
                TaskMutationRequest {
                    idempotency_key: key.into(),
                    entity_id: "plan".into(),
                    revision,
                    mutation,
                },
                OperatingMode::Plan,
                AuthorizationGrant::Explicit,
            )
            .expect("valid plan mutation");
    }
    assert!(matches!(
        service.apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "plan-approve-again".into(),
                entity_id: "plan".into(),
                revision: 4,
                mutation: TaskMutation::PlanApprove,
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        ),
        Err(CapabilityLedgerError::InvalidTaskTransition(_))
    ));

    for (key, revision, mutation) in [
        (
            "goal-budget",
            1,
            TaskMutation::GoalSetBudget { budget: Some(5) },
        ),
        ("goal-consume-a", 2, TaskMutation::GoalConsume { amount: 3 }),
        ("goal-consume-b", 3, TaskMutation::GoalConsume { amount: 2 }),
    ] {
        service
            .apply_task_mutation(
                TaskMutationRequest {
                    idempotency_key: key.into(),
                    entity_id: "goal".into(),
                    revision,
                    mutation,
                },
                OperatingMode::Plan,
                AuthorizationGrant::Explicit,
            )
            .expect("valid goal mutation");
    }
    assert!(matches!(
        service.apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "goal-over-budget".into(),
                entity_id: "goal".into(),
                revision: 4,
                mutation: TaskMutation::GoalConsume { amount: 1 },
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        ),
        Err(CapabilityLedgerError::InvalidTaskTransition(_))
    ));
    service
        .apply_task_mutation(
            TaskMutationRequest {
                idempotency_key: "goal-complete".into(),
                entity_id: "goal".into(),
                revision: 4,
                mutation: TaskMutation::GoalComplete {
                    assurance: slim_core::session::TaskGoalAssurance::Verified,
                },
            },
            OperatingMode::Plan,
            AuthorizationGrant::Explicit,
        )
        .expect("complete goal");
}

#[test]
fn builtin_capabilities_are_immutable_and_kind_policy_is_checked() {
    let mut catalog = CapabilityCatalog::new();
    assert!(matches!(
        catalog.add(CapabilityDescriptor {
            id: "agent.child".into(),
            kind: CapabilityKind::Skill,
            allowed_modes: vec![OperatingMode::Auto, OperatingMode::Plan],
            authorization: AuthorizationRequirement::Explicit,
            replay_policy: ReplayPolicy::Never,
            mutates_workspace: false,
            selection: CapabilitySelection::None,
        }),
        Err(CapabilityLedgerError::InvalidCapabilityPolicy(_))
    ));
    catalog.add_builtin_state_capabilities().expect("builtins");
    assert!(matches!(
        catalog.add_builtin_state_capabilities(),
        Err(CapabilityLedgerError::DuplicateCapability)
    ));
}

#[test]
fn restore_rejects_oversized_fact_key_before_using_payload() {
    let mut repo = MemoryRepo::new(DurableSessionHeader::new(
        "fact-key", "now", "D:\\Slim", None, None,
    ));
    repo.append(DurableRecord::Fact {
        seq: 0,
        fact: DurableFact {
            namespace: "capability.v1".into(),
            key: "k".repeat(MAX_CAPABILITY_QUEUE * MAX_CAPABILITY_QUEUE),
            value: serde_json::json!({
                "schema_version": 1,
                "kind": "unknown",
                "payload": "must not be used"
            }),
        },
    })
    .expect("append fixture");
    assert!(matches!(
        CapabilityService::new(repo, catalog()),
        Err(CapabilityLedgerError::InvalidRecords(_))
    ));
}

#[test]
fn capability_cancellation_is_durable_and_never_dispatches_again() {
    let mut service = CapabilityService::new(
        FaultRepo {
            inner: MemoryRepo::new(DurableSessionHeader::new(
                "cancel-capability",
                "now",
                "D:\\Slim",
                None,
                None,
            )),
            fail_terminal_once: true,
            fail_child_started_once: false,
        },
        catalog(),
    )
    .expect("service");
    let calls = Cell::new(0);
    let _ = service.dispatch(
        CapabilityRequest::new(
            "cancel-q",
            "cancel-exec",
            "skill.review",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        ),
        |_, _| {
            calls.set(calls.get() + 1);
            CapabilityTerminal::Success
        },
    );
    service
        .cancel_capability(
            "cancel-q",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("request cancellation");
    assert_eq!(calls.get(), 1);
    assert_eq!(
        service.capability_state("cancel-q"),
        Some(slim_core::session::CapabilityExecutionState::CancellationRequested)
    );
    let mut restored = CapabilityService::new(service.into_repo(), catalog()).expect("restore");
    assert!(matches!(
        restored.dispatch_queued("cancel-q", |_, _| {
            panic!("cancelled work must not dispatch")
        }),
        Err(CapabilityLedgerError::InFlightRequiresDecision(_))
    ));
    restored
        .finish_capability_cancellation(
            "cancel-q",
            OperatingMode::Auto,
            AuthorizationGrant::Explicit,
        )
        .expect("finish cancellation");
    assert_eq!(
        restored.capability_status("cancel-q"),
        Some(CapabilityTerminal::Cancelled)
    );
    assert_eq!(calls.get(), 1);
}

#[test]
fn restore_reauthorizes_task_and_child_facts() {
    let task_request = TaskMutationRequest {
        idempotency_key: "forged-task".into(),
        entity_id: "todo".into(),
        revision: 1,
        mutation: TaskMutation::TodoAdd {
            title: "task".into(),
        },
    };
    let mut task_repo = MemoryRepo::new(DurableSessionHeader::new(
        "reauth-task",
        "now",
        "D:\\Slim",
        None,
        None,
    ));
    task_repo
        .append(DurableRecord::Fact {
            seq: 0,
            fact: DurableFact {
                namespace: "task.v1".into(),
                key: "forged-task".into(),
                value: serde_json::json!({
                    "schema_version": 1,
                    "kind": "mutation",
                    "request": task_request,
                    "capability_id": "task.todo",
                    "mode": "plan",
                    "authorization": "none",
                }),
            },
        })
        .expect("task fact");
    assert!(matches!(
        CapabilityService::new(task_repo, catalog()),
        Err(CapabilityLedgerError::InvalidRecords(_))
    ));

    let mut seeded = service();
    seed_root(&mut seeded);
    let mut child_repo = seeded.into_repo();
    let mut child_request = ChildRequest::new(
        "forged-child",
        "forged-child-exec",
        "root-exec",
        "capability-test",
        "forged-child-session",
    );
    child_request.depth = 1;
    child_repo
        .append(DurableRecord::Fact {
            seq: child_repo.records().last().expect("root fact").seq() + 1,
            fact: DurableFact {
                namespace: "capability.v1".into(),
                key: "forged-child.child_intent".into(),
                value: serde_json::json!({
                    "schema_version": 1,
                    "kind": "child_intent",
                    "request": child_request,
                    "status": "active",
                    "capability_id": "agent.child",
                    "mode": "auto",
                    "authorization": "none",
                }),
            },
        })
        .expect("child fact");
    assert!(matches!(
        CapabilityService::new(child_repo, catalog()),
        Err(CapabilityLedgerError::InvalidRecords(_))
    ));
}
