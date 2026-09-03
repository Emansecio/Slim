use slim_core::{EventKind, OperatingMode, Runtime};

#[test]
fn direct_tool_lifecycle_has_stable_identity_redacted_arguments_and_duration() {
    let mut runtime = Runtime::new();
    runtime.register_sensitive_value("super-secret-token");
    let root = std::env::temp_dir();

    let (_, next_seq) = runtime
        .execute_tool(
            OperatingMode::ReadOnly,
            &root,
            "read",
            r#"{"path":"super-secret-token"}"#,
            1,
        )
        .expect("failed read still has a truthful lifecycle");
    assert_eq!(next_seq, 4);

    let events = runtime.app.events();
    let EventKind::ToolStarted {
        batch_id,
        call_id,
        arguments,
        ..
    } = &events[0].kind
    else {
        panic!("expected ToolStarted");
    };
    assert!(!batch_id.is_empty());
    assert!(!call_id.is_empty());
    assert!(arguments.contains("[REDACTED]"));
    assert!(!arguments.contains("super-secret-token"));

    assert!(matches!(
        &events[1].kind,
        EventKind::ToolOutput {
            batch_id: output_batch,
            call_id: output_call,
            output,
            ..
        } if output_batch == batch_id
            && output_call == call_id
            && !output.contains("super-secret-token")
    ));
    assert!(matches!(
        &events[2].kind,
        EventKind::ToolFinished {
            batch_id: finished_batch,
            call_id: finished_call,
            name: _,
            success: _,
            duration_ms: _duration_ms,
        } if finished_batch == batch_id && finished_call == call_id
    ));
}
