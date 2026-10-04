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
