use super::*;

#[test]
fn restored_tools_keep_batch_order_and_namespace_reused_call_ids() {
    let call = |id: &str| slim_core::provider::ProviderToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: format!("{{\"path\":\"{id}\"}}"),
    };
    let history = vec![
        ProviderMessage::assistant("checking", vec![call("a"), call("b")]),
        ProviderMessage::tool("read", "b", "second result"),
        ProviderMessage::tool("read", "a", "first result"),
        ProviderMessage::assistant("", vec![call("a")]),
        ProviderMessage::tool("read", "a", "later result"),
    ];
    let messages = transcript_messages(&history);
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1].text, "first result");
    assert_eq!(messages[2].text, "second result");
    assert_eq!(messages[3].text, "later result");
    let ids = messages
        .iter()
        .filter_map(|message| match &message.role {
            TranscriptRole::Tool { call_id, .. } => Some(&call_id.0),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 3);
}

#[test]
fn session_header_decodes_valid_first_line() {
    let path = std::env::temp_dir().join(format!(
        "slim-session-header-ok-{}-{}.jsonl",
        std::process::id(),
        system_time_nanos(SystemTime::now())
    ));
    let header = DurableSessionHeader::new("tui-check", "1", "D:/tmp", None, None);
    let mut bytes = serde_json::to_vec(&header).expect("encode header");
    bytes.push(b'\n');
    bytes.extend_from_slice(b"{\"type\":\"operation\"}");
    fs::write(&path, &bytes).expect("write fixture");
    let parsed = read_session_header(&path).expect("header");
    assert_eq!(parsed.id, "tui-check");
    let _ = fs::remove_file(&path);
}

#[test]
fn session_header_rejects_line_beyond_the_read_cap() {
    let path = std::env::temp_dir().join(format!(
        "slim-session-header-huge-{}-{}.jsonl",
        std::process::id(),
        system_time_nanos(SystemTime::now())
    ));
    let header = DurableSessionHeader::new("tui-huge", "1", "x".repeat(128 * 1024), None, None);
    let mut bytes = serde_json::to_vec(&header).expect("encode header");
    bytes.push(b'\n');
    fs::write(&path, &bytes).expect("write fixture");
    assert!(read_session_header(&path).is_none());
    let _ = fs::remove_file(&path);
}

#[test]
fn job_finished_notice_reads_as_a_sentence_and_hides_a_clean_exit() {
    let job = |state: &str, exit_code, elapsed_ms| slim_core::runtime::ShellJobInfo {
        id: "shell-67".into(),
        command: String::new(),
        origin: "model".into(),
        state: state.into(),
        elapsed_ms,
        exit_code,
        output_bytes: 0,
    };
    assert_eq!(
        super::job_finished_notice(&job("completed", Some(0), 5_300)),
        "shell-67 · concluído · 5s"
    );
    assert_eq!(
        super::job_finished_notice(&job("failed", Some(3), 420)),
        "shell-67 · falhou · exit 3 · 420ms"
    );
    assert_eq!(
        super::job_finished_notice(&job("cancelled", None, 12_000)),
        "shell-67 · cancelado · 12s"
    );
}

#[test]
fn restored_codemode_reconstructs_child_steps_from_journal_facts() {
    use slim_core::session::{
        DurableEntry, DurableEntryRole, DurableFact, DurableRecord, DurableSessionHeader,
        PreflightStatus, SessionFormat, SessionPreflight, SessionSummary,
    };

    let preflight = SessionPreflight {
        path: std::path::PathBuf::from("session.jsonl"),
        format: Some(SessionFormat::DurableV2),
        session_id: Some("s1".into()),
        header: Some(DurableSessionHeader::new("s1", "1", ".", None, None)),
        status: PreflightStatus::Healthy,
        last_seq: Some(5),
        next_seq: Some(6),
        sequence_overflow: false,
        needs_separator: false,
        summary: SessionSummary::default(),
        records: vec![
            DurableRecord::Entry {
                seq: 1,
                entry: DurableEntry {
                    entry_id: "e1".into(),
                    role: DurableEntryRole::User,
                    content: "run".into(),
                    parent_entry_id: None,
                    operation_id: "op1".into(),
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                    content_blocks: Vec::new(),
                },
            },
            DurableRecord::Entry {
                seq: 2,
                entry: DurableEntry {
                    entry_id: "e2".into(),
                    role: DurableEntryRole::Assistant,
                    content: String::new(),
                    parent_entry_id: Some("e1".into()),
                    operation_id: "op1".into(),
                    tool_call_id: None,
                    tool_calls: vec![slim_core::provider::ProviderToolCall {
                        id: "call-1".into(),
                        name: "codemode".into(),
                        arguments: r#"{"code":"await slim.read({path:'a.txt'});"}"#.into(),
                    }],
                    content_blocks: Vec::new(),
                },
            },
            DurableRecord::Fact {
                seq: 3,
                fact: DurableFact {
                    namespace: "codemode.call.v1".into(),
                    key: "batch-1:call-1:1".into(),
                    value: serde_json::json!({
                        "parent_call_id": "call-1",
                        "name": "slim.read",
                        "status": "started",
                        "arguments": {"path": "a.txt"}
                    }),
                },
            },
            DurableRecord::Fact {
                seq: 4,
                fact: DurableFact {
                    namespace: "codemode.call.v1".into(),
                    key: "batch-1:call-1:1".into(),
                    value: serde_json::json!({
                        "parent_call_id": "call-1",
                        "name": "slim.read",
                        "status": "completed",
                        "duration_ms": 12,
                        "result": {"output_bytes": 42}
                    }),
                },
            },
            DurableRecord::Entry {
                seq: 5,
                entry: DurableEntry {
                    entry_id: "e3".into(),
                    role: DurableEntryRole::Tool,
                    content: "\"ok\"".into(),
                    parent_entry_id: Some("e2".into()),
                    operation_id: "op1".into(),
                    tool_call_id: Some("call-1".into()),
                    tool_calls: Vec::new(),
                    content_blocks: Vec::new(),
                },
            },
        ],
    };
    let messages = session_transcript(&preflight).unwrap();
    assert_eq!(messages.len(), 3);
    let TranscriptRole::Tool {
        batch_id: cm_batch,
        name: cm_name,
        ..
    } = &messages[1].role
    else {
        panic!("expected codemode tool");
    };
    let TranscriptRole::Tool {
        batch_id: step_batch,
        name: step_name,
        arguments: step_args,
        ..
    } = &messages[2].role
    else {
        panic!("expected reconstructed step");
    };
    assert_eq!(cm_name, "codemode");
    assert_eq!(step_name, "read");
    assert_eq!(cm_batch, step_batch);
    assert!(step_args.contains("a.txt"));
    assert!(messages[2].text.contains("status: completed (12ms)"));
}
