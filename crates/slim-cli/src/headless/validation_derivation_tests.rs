use super::derive_validated_completion;
use slim_core::{runtime::AgentLoopStop, CausalProgressKind, EventKind, SessionEvent};

fn progress(seq: u64, kind: CausalProgressKind) -> SessionEvent {
    SessionEvent::new(
        seq,
        EventKind::CausalProgressObserved {
            batch_id: "batch".into(),
            call_id: format!("call-{seq}").into(),
            kind,
            tool_name: "shell".into(),
            call_fingerprint: "fingerprint".into(),
            evidence_id: "evidence".into(),
            workspace_revision: seq,
        },
    )
}

#[test]
fn validation_must_follow_the_last_workspace_mutation() {
    let mut events = vec![
        progress(1, CausalProgressKind::WorkspaceChanged),
        progress(2, CausalProgressKind::ValidationGreen),
    ];
    assert!(!derive_validated_completion(
        AgentLoopStop::ProviderCompleted,
        &events
    ));

    events.push(SessionEvent::new(
        3,
        EventKind::GoalAssurance { verified: true },
    ));
    assert!(derive_validated_completion(
        AgentLoopStop::ProviderCompleted,
        &events
    ));

    events.push(progress(4, CausalProgressKind::WorkspaceChanged));
    assert!(!derive_validated_completion(
        AgentLoopStop::ProviderCompleted,
        &events
    ));
}
