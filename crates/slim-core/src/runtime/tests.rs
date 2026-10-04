use super::*;
use crate::context::compaction_prefix_fingerprint;
use serde_json::json;

use super::temp_root::TempRoot;

#[cfg(windows)]
#[test]
fn shell_log_is_redacted_and_paged_by_session_artifact_id() {
    let root = TempRoot::new("shell-log");
    let workspace = root.join("workspace");
    let artifacts = root.join("artifacts");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut runtime = Runtime::with_artifact_store(&artifacts).unwrap();
    runtime.register_sensitive_value("SENSITIVE-LOG-VALUE");
    let (shell, mut seq) = runtime
        .execute_tool(
            crate::OperatingMode::Auto,
            &workspace,
            "shell",
            &json!({"command": "Write-Output (('A' * 9000) + 'SENSITIVE-LOG-VALUE' + 'é' + ('B' * 9000))"}).to_string(),
            1,
        )
        .unwrap();
    assert!(shell.success, "{}", shell.output);
    assert!(shell.output.contains("[truncated "));
    let handle = shell.artifact.expect("complete shell log artifact");
    assert!(!handle.path.starts_with(&workspace));
    let mut offset = 0;
    let mut log = String::new();
    loop {
        let (page, next) = runtime
            .execute_tool(
                crate::OperatingMode::Auto,
                &workspace,
                "artifact_read",
                &json!({"id": handle.id, "offset": offset, "max_bytes": 4096}).to_string(),
                seq,
            )
            .unwrap();
        seq = next;
        assert!(page.success, "{}", page.output);
        let value: serde_json::Value = serde_json::from_str(&page.output).unwrap();
        log.push_str(value["content"].as_str().unwrap());
        offset = value["next_offset"].as_u64().unwrap() as usize;
        if value["eof"] == true {
            break;
        }
    }
    assert!(log.contains("capture_complete=true"));
    assert!(log.contains("[REDACTED]"));
    assert!(log.contains('é'));
    assert!(!log.contains("SENSITIVE-LOG-VALUE"));
    assert!(log.contains(&"A".repeat(9000)));
    assert!(log.contains(&"B".repeat(9000)));

    let advertised = runtime.tool_definition_set(crate::OperatingMode::Auto, false);
    assert!(advertised
        .iter()
        .any(|tool| tool["name"] == "artifact_read"));

    let mut resumed = Runtime::with_artifact_store(&artifacts).unwrap();
    resumed.restore_artifact_ids(std::slice::from_ref(&handle.id));
    let (restored_page, _) = resumed
        .execute_tool(
            crate::OperatingMode::Auto,
            &workspace,
            "artifact_read",
            &json!({"id": handle.id, "max_bytes": 4096}).to_string(),
            1,
        )
        .unwrap();
    assert!(restored_page.success, "{}", restored_page.output);

    let mut other_session = Runtime::with_artifact_store(&artifacts).unwrap();
    let (unlisted, _) = other_session
        .execute_tool(
            crate::OperatingMode::Auto,
            &workspace,
            "artifact_read",
            &json!({"id": handle.id}).to_string(),
            1,
        )
        .unwrap();
    assert!(!unlisted.success);
    assert!(unlisted.output.contains("not available in this session"));

    std::fs::write(&handle.path, "X".repeat(handle.size as usize)).unwrap();
    let (corrupt, _) = runtime
        .execute_tool(
            crate::OperatingMode::Auto,
            &workspace,
            "artifact_read",
            &json!({"id": handle.id}).to_string(),
            seq,
        )
        .unwrap();
    assert!(!corrupt.success);
    assert!(corrupt.output.contains("InvalidData"));
}

#[cfg(windows)]
#[test]
fn then_run_preserves_its_long_validation_output() {
    let root = TempRoot::new("then-run-log");
    let mut runtime = Runtime::with_artifact_store(root.join(".slim/artifacts")).unwrap();
    let (result, seq) = runtime
        .execute_tool(
            crate::OperatingMode::Auto,
            &root,
            "write",
            &json!({
                "path": "edited.txt",
                "content": "edited",
                "then_run": {"command": "Write-Output ('Q' * 12000)"}
            })
            .to_string(),
            1,
        )
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert_eq!(
        std::fs::read_to_string(root.join("edited.txt")).unwrap(),
        "edited"
    );
    let handle = result.artifact.expect("then_run shell log");
    let (page, _) = runtime
        .execute_tool(
            crate::OperatingMode::Auto,
            &root,
            "artifact_read",
            &json!({"id": handle.id, "max_bytes": 16384}).to_string(),
            seq,
        )
        .unwrap();
    assert!(page.success, "{}", page.output);
    assert!(page.output.contains(&"Q".repeat(12000)));
}

#[tokio::test]
async fn manual_retry_rejects_auth_partial_output_and_tool_emission() {
    for event in [
        crate::EventKind::AssistantTextDelta {
            text: "partial".into(),
        },
        crate::EventKind::ReasoningDelta {
            text: "partial".into(),
        },
        crate::EventKind::ProviderToolCall {
            id: "call".into(),
            name: "write".into(),
            arguments: "{}".into(),
        },
    ] {
        let mut runtime = Runtime::new();
        let handle = ManualRetryHandle::default();
        runtime.set_manual_retry_handle(handle.clone());
        let mut seq = 1;
        push_runtime_event(&mut runtime.app, &mut seq, event).unwrap();
        let error = ProviderError::Http {
            status: 503,
            retry_after: None,
            message: "failed".into(),
        };
        assert!(!runtime
            .wait_for_manual_retry(&error, 0, DEFAULT_BACKOFF, &mut seq)
            .await
            .unwrap());
        assert!(!handle.request());
    }
    let mut runtime = Runtime::new();
    let handle = ManualRetryHandle::default();
    runtime.set_manual_retry_handle(handle.clone());
    let mut seq = 1;
    let error = ProviderError::Http {
        status: 401,
        retry_after: None,
        message: "denied".into(),
    };
    assert!(!runtime
        .wait_for_manual_retry(&error, 0, DEFAULT_BACKOFF, &mut seq)
        .await
        .unwrap());
    assert!(!handle.request());
}

#[tokio::test]
async fn manual_retry_wait_is_cancelled_and_does_not_bypass_retry_after() {
    for request_retry in [false, true] {
        let mut runtime = Runtime::new();
        let handle = ManualRetryHandle::default();
        let token = CancellationToken::new();
        runtime.set_manual_retry_handle(handle.clone());
        runtime.set_cancellation_token(token.clone());
        let error = ProviderError::Http {
            status: 429,
            retry_after: Some(std::time::Duration::from_secs(120)),
            message: "later".into(),
        };
        let mut seq = 1;
        let waiting = runtime.wait_for_manual_retry(&error, 0, DEFAULT_BACKOFF, &mut seq);
        let cancel = async {
            while !handle.is_waiting() {
                tokio::task::yield_now().await;
            }
            if request_retry {
                assert!(handle.request());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            token.cancel();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(waiting, cancel)
        })
        .await
        .expect("cancel must interrupt waiting and server delay");
        assert!(!result.unwrap(), "retry cannot run before Retry-After");
        assert!(!handle.request());
    }
}

#[test]
fn secret_gate_uses_call_identity_and_decoded_arguments() {
    let events = [
        ProviderEvent::ToolCallDelta {
            index: Some(7),
            id: Some("call".into()),
            name: Some("read".into()),
            arguments: "{\"path\":\"secret-".into(),
        },
        ProviderEvent::ToolCallDelta {
            index: None,
            id: Some("call".into()),
            name: Some("read".into()),
            arguments: "value\"}".into(),
        },
    ];
    assert!(tool_events_contain_sensitive_values(
        &events,
        &["secret-value".into()]
    ));
    assert!(!tool_events_contain_sensitive_values(
        &events,
        &["callcall".into(), "readread".into()]
    ));
    assert!(sensitive_tool_arguments(
        r#"{"path":"secret\u002dvalue"}"#,
        &["secret-value".into()]
    ));
}

#[test]
fn canonical_text_is_independent_of_chunk_boundaries() {
    let source = "```python\r\nvalue = 1\r\n\tprint('á € 🦀')\r\n```\n synthetic-secret-value \n";
    let expected = source.replace("synthetic-secret-value", "[REDACTED]");
    let boundaries = source
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(source.len()))
        .collect::<Vec<_>>();
    for split in boundaries {
        let mut app = AppHandle::fake();
        let mut normalizer = ProviderStreamNormalizer::new(
            ProviderKind::OpenAiCompatible,
            1,
            vec!["synthetic-secret-value".into()],
        );
        for chunk in [&source[..split], &source[split..]] {
            normalizer.push(&mut app, ProviderEvent::TextDelta(chunk.into()));
        }
        normalizer.flush_text(&mut app).unwrap();
        let actual = app
            .events()
            .iter()
            .filter_map(|event| match &event.kind {
                crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(actual, expected, "split {split}");
    }
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(
        ProviderKind::OpenAiCompatible,
        1,
        vec!["synthetic-secret-value".into()],
    );
    for ch in source.chars() {
        normalizer.push(&mut app, ProviderEvent::TextDelta(ch.to_string()));
    }
    normalizer.flush_text(&mut app).unwrap();
    let actual = app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(actual, expected);
}

#[test]
fn reasoning_classification_is_emitted_before_public_reasoning_text() {
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCodex, 1, vec![])
        .with_reasoning_classification(Some(crate::ReasoningClassification::Summary));
    normalizer.push(&mut app, ProviderEvent::ReasoningDelta("summary".into()));
    let kinds = app
        .events()
        .iter()
        .map(|event| match &event.kind {
            crate::EventKind::ReasoningClassification { classification } => {
                format!("classification:{classification:?}")
            }
            crate::EventKind::ThinkingStarted => "thinking_started".into(),
            crate::EventKind::ReasoningDelta { .. } => "reasoning_delta".into(),
            _ => "other".into(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            "classification:Summary",
            "thinking_started",
            "reasoning_delta"
        ]
    );
}

#[test]
fn generic_reasoning_stream_does_not_invent_a_classification() {
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec![]);
    normalizer.push(&mut app, ProviderEvent::ReasoningDelta("opaque".into()));
    assert!(!app
        .events()
        .iter()
        .any(|event| matches!(event.kind, crate::EventKind::ReasoningClassification { .. })));
}

#[tokio::test]
async fn journal_failure_cancels_and_drains_started_mutations() {
    use crate::session::{DurableSessionHeader, JsonlRepo, ManualRunJournal, ManualRunSpec};
    let root = TempRoot::new("drain");
    for name in ["first.txt", "waiting.txt"] {
        std::fs::write(root.join(name), "before").unwrap();
    }
    let first_lock = std::fs::File::open(root.join("first.txt")).unwrap();
    let waiting_lock = std::fs::File::open(root.join("waiting.txt")).unwrap();
    first_lock.lock().unwrap();
    waiting_lock.lock().unwrap();
    let calls = ["first.txt", "waiting.txt"]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            provider_call(
                &format!("write-{i}"),
                "write",
                json!({"path":name,"content":"after","expected":"before"}),
            )
        })
        .collect::<Vec<_>>();
    let repo = JsonlRepo::create(
        root.join("session.jsonl"),
        DurableSessionHeader::new("drain", "now", root.to_str().unwrap(), None, None),
    )
    .unwrap();
    let mut journal = ManualRunJournal::start(
        repo,
        ManualRunSpec::new("op", "attempt", "input", "final", "write", 0),
    )
    .unwrap();
    journal
        .begin_tools(
            "batch",
            ProviderMessage::assistant("", calls.clone()),
            &calls,
        )
        .unwrap();
    let store = ArtifactStore::new(root.join("blocked")).unwrap();
    std::fs::write(root.join("blocked"), "not a directory").unwrap();
    journal.configure_output(Some(store), 0);
    let mut runtime = Runtime::new();
    runtime.app.run_journal = Some(Arc::new(Mutex::new(journal)));
    let token = CancellationToken::new();
    runtime.cancellation = Some(token.clone());
    let mut governor = CausalGovernor::default();
    let work = runtime.execute_provider_tool_batch(
        crate::OperatingMode::Auto,
        &root,
        "batch",
        &calls,
        1,
        &mut governor,
    );
    let release = async {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while token.0.native_work.load(Ordering::Acquire) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        first_lock.unlock().unwrap();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(work, release)
    })
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(token.is_cancelled());
    assert_eq!(token.0.native_work.load(Ordering::Acquire), 0);
    waiting_lock.unlock().unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("first.txt")).unwrap(),
        "after"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("waiting.txt")).unwrap(),
        "before"
    );
    assert_eq!(
        runtime
            .app
            .events()
            .iter()
            .filter(|event| matches!(event.kind, crate::EventKind::ToolFinished { .. }))
            .count(),
        2
    );
    drop((first_lock, waiting_lock, runtime));
}

#[tokio::test]
async fn ordinary_mcp_configuration_preserves_native_arguments_and_durable_ids() {
    use crate::mcp::McpTransport;
    use crate::session::{DurableSessionHeader, JsonlRepo, ManualRunJournal, ManualRunSpec};
    let transport = McpTransport::Stdio {
        command: "unused".into(),
        args: vec![],
        env: [
            ("WORKERS", "1"),
            ("ENABLED", "true"),
            ("NODE_ENV", "production"),
            ("SERVICE_API_TOKEN", "synthetic-credential-long-42"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect(),
    };
    let secrets = transport.sensitive_values().cloned().collect::<Vec<_>>();
    assert_eq!(secrets, ["synthetic-credential-long-42"]);
    let root = TempRoot::new("ordinary-config");
    std::fs::write(root.join("production.json"), "first\nsecond\n").unwrap();
    let mut runtime = Runtime::new();
    for secret in &secrets {
        runtime.register_sensitive_value(secret);
    }
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, secrets);
    let args = r#"{"path":"production.json","offset":1,"max_lines":20}"#;
    for i in 0..2 {
        normalizer.push(
            &mut runtime.app,
            ProviderEvent::ToolCallDelta {
                index: Some(i),
                id: Some(format!("call-{i}")),
                name: Some("read".into()),
                arguments: args.into(),
            },
        );
    }
    normalizer.push(
        &mut runtime.app,
        ProviderEvent::Stopped {
            reason: "tool_calls".into(),
        },
    );
    let turn = normalizer.finish(&mut runtime.app).unwrap();
    let calls = tool_calls_since(&runtime.app, 0);
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(
        |call| serde_json::from_str::<Value>(&call.arguments).unwrap()
            == serde_json::from_str::<Value>(args).unwrap()
    ));
    let repo = JsonlRepo::create(
        root.join("session.jsonl"),
        DurableSessionHeader::new("ordinary", "now", root.to_str().unwrap(), None, None),
    )
    .unwrap();
    let mut journal = ManualRunJournal::start(
        repo,
        ManualRunSpec::new("op", "attempt", "input", "final", "read", 0),
    )
    .unwrap();
    journal
        .begin_tools(
            "batch",
            ProviderMessage::assistant("", calls.clone()),
            &calls,
        )
        .unwrap();
    runtime.app.run_journal = Some(Arc::new(Mutex::new(journal)));
    let (results, _) = runtime
        .execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "batch",
            &calls,
            turn.next_seq,
            &mut CausalGovernor::default(),
        )
        .await
        .unwrap();
    assert!(results
        .iter()
        .all(|result| result.success && result.output.contains("second")));
    drop(runtime);
    let durable = std::fs::read_to_string(root.join("session.jsonl")).unwrap();
    assert!(durable.contains("call-0") && durable.contains("call-1"));
    assert!(!durable.contains("synthetic-credential-long-42"));
    // The batch's wall time is journaled once, naming every call it ran.
    let batches: Vec<serde_json::Value> = durable
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| record["fact"]["namespace"] == "tool.batch.v1")
        .map(|record| record["fact"]["value"].clone())
        .collect();
    assert_eq!(batches.len(), 1, "{durable}");
    assert_eq!(batches[0]["batch_id"], "batch");
    assert_eq!(batches[0]["calls"], 2);
    assert_eq!(
        batches[0]["call_ids"],
        serde_json::json!(["call-0", "call-1"])
    );
    assert!(batches[0]["wall_ms"].is_u64());
}

#[test]
fn empty_content_and_placeholder_tool_delta_do_not_split_reasoning() {
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec![]);
    for event in [
        ProviderEvent::ReasoningDelta("first".into()),
        ProviderEvent::TextDelta(String::new()),
        ProviderEvent::TextDelta(" \n ".into()),
        ProviderEvent::ToolCallDelta {
            index: Some(0),
            id: None,
            name: None,
            arguments: String::new(),
        },
        ProviderEvent::ReasoningDelta(" second".into()),
        ProviderEvent::Stopped {
            reason: "stop".into(),
        },
    ] {
        normalizer.push(&mut app, event);
    }
    normalizer.finish(&mut app).expect("normal stop");
    let kinds: Vec<_> = app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ThinkingStarted => Some("start"),
            crate::EventKind::ReasoningDelta { text } => Some(text.as_str()),
            crate::EventKind::ThinkingEnded => Some("end"),
            crate::EventKind::AssistantTextDelta { .. } => Some("text"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["start", "first", "text", " second", "end"]);
    assert!(app.events().iter().any(|event| matches!(&event.kind,
            crate::EventKind::AssistantTextDelta { text } if text == " \n ")));
}

#[test]
fn argument_fragment_does_not_split_reasoning() {
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec![]);
    for event in [
        ProviderEvent::ReasoningDelta("first".into()),
        ProviderEvent::ToolCallDelta {
            index: Some(0),
            id: None,
            name: None,
            arguments: "{".into(),
        },
        ProviderEvent::ReasoningDelta(" second".into()),
        ProviderEvent::Stopped {
            reason: "stop".into(),
        },
    ] {
        normalizer.push(&mut app, event);
    }
    normalizer.finish(&mut app).expect("normal stop");
    let kinds: Vec<_> = app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ThinkingStarted => Some("start"),
            crate::EventKind::ReasoningDelta { text } => Some(text.as_str()),
            crate::EventKind::ThinkingEnded => Some("end"),
            crate::EventKind::AssistantTextDelta { .. } => Some("text"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["start", "first second", "end"]);
}

#[test]
fn reasoning_then_text_keeps_a_single_thought() {
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec![]);
    for event in [
        ProviderEvent::ReasoningDelta("plan".into()),
        ProviderEvent::TextDelta("answer".into()),
        ProviderEvent::Stopped {
            reason: "stop".into(),
        },
    ] {
        normalizer.push(&mut app, event);
    }
    normalizer.finish(&mut app).expect("normal stop");
    let kinds: Vec<_> = app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ThinkingStarted => Some("start"),
            crate::EventKind::ReasoningDelta { text } => Some(text.as_str()),
            crate::EventKind::ThinkingEnded => Some("end"),
            crate::EventKind::AssistantTextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["start", "plan", "end", "answer"]);
}

#[test]
fn identical_stop_reason_is_idempotent() {
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec![]);
    normalizer.push(&mut app, ProviderEvent::TextDelta("done".into()));
    normalizer.push(
        &mut app,
        ProviderEvent::Stopped {
            reason: "stop".into(),
        },
    );
    normalizer.push(
        &mut app,
        ProviderEvent::Stopped {
            reason: "Stop".into(),
        },
    );
    normalizer.finish(&mut app).expect("identical stop");
    assert_eq!(
        app.events()
            .iter()
            .filter(|event| matches!(event.kind, crate::EventKind::AssistantEnded { .. }))
            .count(),
        1
    );
}

#[test]
fn conflicting_stop_reasons_are_rejected() {
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec![]);
    normalizer.push(&mut app, ProviderEvent::TextDelta("done".into()));
    normalizer.push(
        &mut app,
        ProviderEvent::Stopped {
            reason: "tool_calls".into(),
        },
    );
    normalizer.push(
        &mut app,
        ProviderEvent::Stopped {
            reason: "stop".into(),
        },
    );
    match normalizer.finish(&mut app) {
        Err(ProviderError::InvalidResponse { message })
            if message.contains("more than one stop reason") => {}
        Err(error) => panic!("expected conflicting stop, got {error:?}"),
        Ok(_) => panic!("expected conflicting stop, got success"),
    }
}

#[test]
fn unused_named_slot_without_id_does_not_reject_a_complete_sibling() {
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec![]);
    normalizer.push(
        &mut app,
        ProviderEvent::ToolCallDelta {
            index: Some(0),
            id: Some("call-a".into()),
            name: Some("read".into()),
            arguments: r#"{"path":"README.md"}"#.into(),
        },
    );
    normalizer.push(
        &mut app,
        ProviderEvent::ToolCallDelta {
            index: Some(1),
            id: None,
            name: Some("read".into()),
            arguments: String::new(),
        },
    );
    normalizer.push(
        &mut app,
        ProviderEvent::Stopped {
            reason: "tool_calls".into(),
        },
    );
    normalizer
        .finish(&mut app)
        .expect("named padding without identity is unused");
    let published: Vec<_> = app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ProviderToolCall {
                id,
                name,
                arguments,
            } => Some((id.as_str(), name.as_str(), arguments.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(published, [("call-a", "read", r#"{"path":"README.md"}"#)]);
}

#[test]
fn unused_openai_tool_slot_does_not_reject_a_complete_sibling() {
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec![]);
    normalizer.push(
        &mut app,
        ProviderEvent::ToolCallDelta {
            index: Some(0),
            id: Some("call-a".into()),
            name: Some("read".into()),
            arguments: r#"{"path":"README.md"}"#.into(),
        },
    );
    normalizer.push(
        &mut app,
        ProviderEvent::ToolCallDelta {
            index: Some(1),
            id: None,
            name: Some(String::new()),
            arguments: String::new(),
        },
    );
    normalizer.push(
        &mut app,
        ProviderEvent::Stopped {
            reason: "tool_calls".into(),
        },
    );
    normalizer.finish(&mut app).expect("complete sibling runs");
    let published: Vec<_> = app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ProviderToolCall {
                id,
                name,
                arguments,
            } => Some((id.as_str(), name.as_str(), arguments.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(published, [("call-a", "read", r#"{"path":"README.md"}"#)]);
}

#[test]
fn empty_name_fragment_does_not_malform_an_identified_call() {
    let mut app = AppHandle::fake();
    let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec![]);
    normalizer.push(
        &mut app,
        ProviderEvent::ToolCallDelta {
            index: Some(0),
            id: Some("call-a".into()),
            name: Some("list".into()),
            arguments: r#"{"path":"C:\\Users"}"#.into(),
        },
    );
    normalizer.push(
        &mut app,
        ProviderEvent::ToolCallDelta {
            index: Some(0),
            id: Some("call-a".into()),
            name: Some(String::new()),
            arguments: String::new(),
        },
    );
    normalizer.push(
        &mut app,
        ProviderEvent::Stopped {
            reason: "tool_calls".into(),
        },
    );
    normalizer.finish(&mut app).expect("empty name is ignored");
    assert!(app.events().iter().any(|event| matches!(
        &event.kind,
        crate::EventKind::ProviderToolCall { name, .. } if name == "list"
    )));
}

#[test]
fn fenced_and_double_encoded_arguments_are_accepted() {
    for arguments in [
        "```json\n{\"path\":\"a.txt\"}\n```",
        "\"{\\\"path\\\":\\\"a.txt\\\"}\"",
    ] {
        let mut app = AppHandle::fake();
        let mut normalizer =
            ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec![]);
        normalizer.push(
            &mut app,
            ProviderEvent::ToolCallDelta {
                index: Some(0),
                id: Some("call-a".into()),
                name: Some("read".into()),
                arguments: arguments.into(),
            },
        );
        normalizer.push(
            &mut app,
            ProviderEvent::Stopped {
                reason: "tool_calls".into(),
            },
        );
        normalizer
            .finish(&mut app)
            .unwrap_or_else(|error| panic!("accepted {arguments:?}: {error:?}"));
        assert!(
            app.events().iter().any(|event| matches!(
                &event.kind,
                crate::EventKind::ProviderToolCall { arguments, .. }
                    if arguments == r#"{"path":"a.txt"}"#
            )),
            "{arguments}"
        );
    }
}

#[test]
fn invalid_json_escapes_are_repaired_without_touching_valid_ones() {
    let normalized = |raw: &str| object_arguments(raw).0.into_owned();
    assert_eq!(
        normalized(r#"{"path":"C:\Slim\src"}"#),
        r#"{"path":"C:\\Slim\\src"}"#
    );
    assert_eq!(
        normalized(r#"{"content":"a\*b"}"#),
        r#"{"content":"a\\*b"}"#
    );
    for valid in [
        r#"{"text":"line\nbreak"}"#,
        r#"{"text":"tab\there"}"#,
        r#"{"text":"quote\"inside"}"#,
        r#"{"path":"C:\\Slim"}"#,
        r#"{"text":"slash\/here"}"#,
        r#"{"text":"caf\u00e9"}"#,
    ] {
        assert_eq!(normalized(valid), valid, "{valid}");
    }
}

#[test]
fn fenced_and_double_encoded_escapes_are_repaired_at_the_inner_layer() {
    assert_eq!(
        object_arguments("```json\n{\"path\":\"a.txt\"}\n```")
            .0
            .as_ref(),
        r#"{"path":"a.txt"}"#
    );
    let double_encoded = r#""{\"content\":\"literal \\* star\"}""#;
    assert_eq!(
        object_arguments(double_encoded).0.as_ref(),
        r#"{"content":"literal \\* star"}"#
    );
}

#[test]
fn object_arguments_matches_the_multi_parse_normalization() {
    fn reference(raw: &str) -> String {
        let trimmed = tool_arguments::strip_json_fence(raw.trim());
        if let Ok(Value::String(inner)) = serde_json::from_str::<Value>(trimmed) {
            if serde_json::from_str::<Value>(&inner).is_ok_and(|value| value.is_object()) {
                return inner;
            }
            if let Some(repaired) = tool_arguments::repaired_json_object(&inner) {
                return repaired;
            }
        }
        if serde_json::from_str::<Value>(trimmed).is_ok_and(|value| value.is_object()) {
            return trimmed.to_owned();
        }
        tool_arguments::repaired_json_object(trimmed).unwrap_or_else(|| trimmed.to_owned())
    }
    let cases = [
        r#"{"path":"a.txt"}"#,
        r#"  {"path":"a.txt"}  "#,
        "```json\n{\"path\":\"a.txt\"}\n```",
        "```JSON {\"path\":\"a.txt\"}```",
        "```\n{\"path\":\"a.txt\"}",
        r#""{\"path\":\"a.txt\"}""#,
        r#""{\"content\":\"literal \\* star\"}""#,
        r#""{\"content\":\"lone \\q\"}""#,
        r#""not json""#,
        r#""[1,2]""#,
        r#""""#,
        r#"{"path":"C:\Slim\src"}"#,
        r#"{"a":"\u12","b":"\x"}"#,
        r#"{"path":"a.txt""#,
        r#"{"path":"a.txt"} tail"#,
        r#"{"path":1e999}"#,
        r#"["a"]"#,
        r#"[1,2"#,
        "42",
        "null",
        "true",
        "",
        "   ",
        "not json at all",
        r#""\q""#,
        r#""{\"a\":1} extra""#,
    ];
    for raw in cases {
        let (text, shape) = object_arguments(raw);
        assert_eq!(text.as_ref(), reference(raw), "{raw}");
        let parsed = serde_json::from_str::<Value>(text.as_ref());
        assert_eq!(
            shape.is_object(),
            parsed.as_ref().is_ok_and(Value::is_object),
            "{raw}"
        );
        let expected_issue = match &parsed {
            Ok(value) if value.is_object() => None,
            Ok(_) => Some("arguments must be a JSON object".to_owned()),
            Err(error) => Some(error.to_string()),
        };
        assert_eq!(shape.issue(), expected_issue, "{raw}");
        assert_eq!(
            validate_tool_arguments("read", raw).is_ok(),
            shape.is_object(),
            "{raw}"
        );
        assert!(validate_tool_arguments(" ", raw).is_err(), "{raw}");
    }
}

#[test]
fn anthropic_duplicate_completed_call_ids_reject_the_entire_batch() {
    for duplicate in [false, true] {
        let mut app = AppHandle::fake();
        let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::Anthropic, 1, vec![]);
        for index in 0..2 {
            normalizer.push(
                &mut app,
                ProviderEvent::ToolCallStart {
                    index,
                    id: if duplicate {
                        "same-call".into()
                    } else {
                        format!("call-{index}")
                    },
                    name: "read".into(),
                },
            );
            normalizer.push(
                &mut app,
                ProviderEvent::ToolCallInputDelta {
                    index,
                    partial_json: format!(r#"{{"path":"file-{index}"}}"#),
                },
            );
            normalizer.push(&mut app, ProviderEvent::ContentBlockStop { index });
        }
        normalizer.push(
            &mut app,
            ProviderEvent::Stopped {
                reason: "tool_use".into(),
            },
        );
        let result = normalizer.finish(&mut app);
        let published = app
            .events()
            .iter()
            .filter(|event| matches!(event.kind, crate::EventKind::ProviderToolCall { .. }))
            .count();
        if duplicate {
            assert!(matches!(result, Err(ProviderError::MalformedToolCall)));
            assert_eq!(published, 0, "no member of an invalid batch may execute");
        } else {
            assert!(
                result.is_ok(),
                "same-name calls with distinct IDs remain valid"
            );
            assert_eq!(published, 2);
        }
    }
}

#[test]
fn protocol_failure_retains_later_usage_without_publishing_more_content() {
    for partial in [false, true] {
        let mut app = AppHandle::fake();
        app.push_event(crate::SessionEvent::new(
            1,
            crate::EventKind::ContextSnapshot {
                request_kind: crate::RequestKind::ProviderTurn,
                provider: "fixture".into(),
                model: "fixture".into(),
                system_bytes: 0,
                tool_schema_bytes: 0,
                history_bytes: 0,
                tool_result_bytes: 0,
                serialized_chars: 0,
                estimated_tokens: 0,
                context_window_tokens: 0,
            },
        ))
        .unwrap();
        let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCodex, 2, vec![]);
        normalizer.push(&mut app, ProviderEvent::TextDelta("Preserved text".into()));
        normalizer.push(
            &mut app,
            ProviderEvent::ToolCallDelta {
                index: Some(0),
                id: Some("call-a".into()),
                name: Some("read".into()),
                arguments: r#"{"path":"a"}"#.into(),
            },
        );
        normalizer.push(
            &mut app,
            ProviderEvent::ToolCallComplete {
                index: 0,
                id: "call-a".into(),
                name: "read".into(),
                arguments: r#"{"path":"b"}"#.into(),
            },
        );
        assert_eq!(normalizer.error, Some(ProviderError::MalformedToolCall));
        normalizer.push(&mut app, ProviderEvent::TextDelta("Rejected text".into()));
        normalizer.push(
            &mut app,
            ProviderEvent::UsageBreakdown {
                usage: crate::provider::UsageBreakdown {
                    uncached_input_tokens: 7,
                    output_tokens: 3,
                    ..crate::provider::UsageBreakdown::default()
                },
            },
        );
        if partial {
            normalizer.push(
                &mut app,
                ProviderEvent::UsagePartial {
                    input_tokens: 7,
                    output_tokens: 0,
                    input_complete: true,
                    output_complete: false,
                },
            );
            normalizer.push(
                &mut app,
                ProviderEvent::UsagePartial {
                    input_tokens: 0,
                    output_tokens: 3,
                    input_complete: false,
                    output_complete: true,
                },
            );
        }
        let terminal = ProviderEvent::Usage {
            input_tokens: if partial { 0 } else { 7 },
            output_tokens: if partial { 0 } else { 3 },
        };
        normalizer.push(&mut app, terminal.clone());
        normalizer.push(&mut app, terminal);
        normalizer.push(
            &mut app,
            ProviderEvent::Stopped {
                reason: "completed".into(),
            },
        );
        let next_seq = normalizer.next_seq();
        assert!(matches!(
            normalizer.finish(&mut app),
            Err(ProviderError::MalformedToolCall)
        ));
        app.push_event(crate::SessionEvent::new(
            next_seq,
            crate::EventKind::RequestCompleted {
                provider_latency_ms: 1,
                cancelled: false,
                failed: true,
            },
        ))
        .unwrap();
        let usage = UsageTotals::from_events(app.events(), false);
        assert_eq!(usage.uncached_input_tokens, 7);
        assert_eq!(usage.output_tokens, 3);
        assert!(!usage.usage_unknown);
        assert!(usage.requests[0].failed);
        assert!(!usage.validated_completion);
        assert!(app.events().iter().any(|event| matches!(
            &event.kind, crate::EventKind::AssistantTextDelta { text }
                if text == "Preserved text"
        )));
        assert!(!app.events().iter().any(|event| matches!(
            &event.kind,
            crate::EventKind::ProviderToolCall { .. }
                | crate::EventKind::ToolStarted { .. }
                | crate::EventKind::AssistantEnded { .. }
        ) || matches!(&event.kind, crate::EventKind::AssistantTextDelta { text }
                if text.contains("Rejected text"))));
    }
}

#[tokio::test]
async fn cancellation_waits_for_detached_native_work_to_finish() {
    let cancellation = CancellationToken::new();
    let native_work = cancellation.track_native_work();
    cancellation.cancel();
    assert!(tokio::time::timeout(
        std::time::Duration::ZERO,
        cancellation.wait_for_native_work()
    )
    .await
    .is_err());
    drop(native_work);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        cancellation.wait_for_native_work(),
    )
    .await
    .unwrap();
}

#[test]
fn argument_repair_requires_a_complete_identified_batch_on_each_wire() {
    let bad = "{\"path\":\"secret-value\"".to_owned();
    for kind in [
        ProviderKind::OpenAiCompatible,
        ProviderKind::OpenAiCodex,
        ProviderKind::Anthropic,
    ] {
        let mut app = AppHandle::fake();
        let mut normalizer = ProviderStreamNormalizer::new(kind, 1, vec!["secret-value".into()]);
        let events = match kind {
            ProviderKind::Anthropic => vec![
                ProviderEvent::ToolCallStart {
                    index: 0,
                    id: "known".into(),
                    name: "read".into(),
                },
                ProviderEvent::ToolCallInputDelta {
                    index: 0,
                    partial_json: bad.clone(),
                },
                ProviderEvent::ContentBlockStop { index: 0 },
            ],
            ProviderKind::OpenAiCodex => vec![ProviderEvent::ToolCallComplete {
                index: 0,
                id: "known".into(),
                name: "read".into(),
                arguments: bad.clone(),
            }],
            _ => vec![ProviderEvent::ToolCallDelta {
                index: Some(0),
                id: Some("known".into()),
                name: Some("read".into()),
                arguments: bad.clone(),
            }],
        };
        for event in events {
            normalizer.push(&mut app, event);
        }
        assert!(
            normalizer.argument_repair_note().is_none(),
            "incomplete transport cannot be repaired"
        );
        let reason = match kind {
            ProviderKind::Anthropic => "tool_use",
            ProviderKind::OpenAiCodex => "completed",
            _ => "tool_calls",
        };
        normalizer.push(
            &mut app,
            ProviderEvent::Stopped {
                reason: reason.into(),
            },
        );
        let note = normalizer
            .argument_repair_note()
            .expect("identified invalid JSON");
        assert!(note.contains("known") && note.contains("[REDACTED]"));
        assert!(!note.contains("secret-value"));
        assert!(
            matches!(
                normalizer.finish(&mut app),
                Err(ProviderError::MalformedToolCall)
            ),
            "one-shot API remains fail closed"
        );
        assert!(app
            .events()
            .iter()
            .all(|event| !matches!(event.kind, crate::EventKind::ProviderToolCall { .. })));
    }
}

#[test]
fn anthropic_repairs_identified_invalid_json_after_end_turn_without_block_stop() {
    let mut app = AppHandle::fake();
    let mut normalizer =
        ProviderStreamNormalizer::new(ProviderKind::Anthropic, 1, vec!["secret-value".into()]);
    for event in [
        ProviderEvent::ToolCallStart {
            index: 0,
            id: "known".into(),
            name: "read".into(),
        },
        ProviderEvent::ToolCallInputDelta {
            index: 0,
            partial_json: "{\"path\":\"secret-value\"".into(),
        },
        ProviderEvent::Stopped {
            reason: "end_turn".into(),
        },
    ] {
        normalizer.push(&mut app, event);
    }
    let note = normalizer
        .argument_repair_note()
        .expect("terminal Anthropic call can be repaired");
    assert!(note.contains("known") && note.contains("[REDACTED]"));
    assert!(!note.contains("secret-value"));
    assert!(matches!(
        normalizer.finish(&mut app),
        Err(ProviderError::MalformedToolCall)
    ));
    assert!(app
        .events()
        .iter()
        .all(|event| !matches!(event.kind, crate::EventKind::ProviderToolCall { .. })));
}

#[test]
fn context_overflow_does_not_match_auth_usage_or_output_limits() {
    for (status, message) in [
        (401, "Access token expired"),
        (403, "Token does not have permission"),
        (429, "Token rate limit exceeded"),
        (400, "max_tokens must be at least 1"),
        (400, "Invalid content length"),
    ] {
        let error = ProviderError::Http {
            status,
            retry_after: None,
            message: message.into(),
        };
        assert!(
            !is_context_overflow_error(&error),
            "not context overflow: {error:?}"
        );
    }
    assert!(is_context_overflow_error(&ProviderError::Http {
        status: 400,
        retry_after: None,
        message: "This model's maximum context length is 1000 tokens".into(),
    }));
}

#[test]
fn context_overflow_follows_pis_patterns_and_the_providers_own_codes() {
    let api = |status: Option<u16>, code: Option<&str>, message: &str| ProviderError::Api {
        metadata: Box::new(crate::provider::ProviderErrorMetadata {
            status,
            code: code.map(str::to_owned),
            error_type: None,
            detail_code: None,
            retry_after: None,
        }),
        message: message.into(),
    };
    // Pi's patterns match the text of any invalid request.
    for error in [
        api(
            Some(400),
            None,
            "prompt is too long: 213462 tokens > 200000 maximum",
        ),
        api(Some(413), None, "request_too_large"),
        api(
            Some(400),
            Some("invalid_request_error"),
            "Your input exceeds the context window of this model",
        ),
        api(
            None,
            None,
            "Input length (265330) exceeds model's maximum context length (262144).",
        ),
        ProviderError::Remote {
            message: "Prompt exceeds max length".into(),
        },
    ] {
        assert!(is_context_overflow_error(&error), "{error:?}");
    }
    // The provider's own code needs no matching text.
    assert!(is_context_overflow_error(&api(
        Some(400),
        Some("context_length_exceeded"),
        "rejected"
    )));
    // Throttling and rate limits are never overflow, even with Pi's generic
    // fallback wording; neither is any status but the invalid-request ones.
    assert!(!is_context_overflow_error(&api(
        Some(429),
        None,
        "Rate limit: too many tokens per minute"
    )));
    assert!(!is_context_overflow_error(&api(
        Some(400),
        None,
        "Throttling error: Too many tokens, please wait"
    )));
    assert!(!is_context_overflow_error(&api(
        Some(500),
        None,
        "prompt is too long"
    )));
    // Pi has no status gate: any client error whose text says overflow is one.
    assert!(is_context_overflow_error(&api(
        Some(424),
        None,
        "prompt is too long"
    )));
    assert!(!is_context_overflow_error(&api(
        Some(408),
        None,
        "prompt is too long"
    )));
    assert!(!is_context_overflow_error(&ProviderError::Cancelled));
}

const DEFAULT_BACKOFF: std::time::Duration = AgentLoopConfig::DEFAULT_PROVIDER_RECOVERY_BACKOFF;

#[test]
fn recovery_backoff_doubles_from_the_configured_base_and_caps_at_sixteen_times() {
    let base = std::time::Duration::from_millis(3);
    assert_eq!(
        provider_recovery_backoff(DEFAULT_BACKOFF, 1).as_millis(),
        500
    );
    assert_eq!(
        provider_recovery_backoff(DEFAULT_BACKOFF, 2).as_millis(),
        1000
    );
    assert_eq!(provider_recovery_backoff(base, 1), base);
    assert_eq!(provider_recovery_backoff(base, 3), base * 4);
    assert_eq!(provider_recovery_backoff(base, 9), base * 16);
    let error = ProviderError::Http {
        status: 503,
        retry_after: Some(std::time::Duration::from_secs(1)),
        message: "busy".into(),
    };
    assert_eq!(
        requested_provider_recovery_delay(&error, 1, base),
        std::time::Duration::from_secs(1)
    );
}

#[test]
fn retry_wait_budget_is_shared_across_attempts() {
    let delay = std::time::Duration::from_secs(40);
    let error = ProviderError::Http {
        status: 429,
        retry_after: Some(delay),
        message: "rate limited".into(),
    };
    assert_eq!(
        provider_recovery_delay(&error, 1, std::time::Duration::ZERO, DEFAULT_BACKOFF).unwrap(),
        delay
    );
    let blocked = provider_recovery_delay(&error, 2, delay, DEFAULT_BACKOFF).unwrap_err();
    assert!(
        matches!(blocked, ProviderError::Http { retry_after: Some(actual), message, .. }
            if actual == delay && message.contains("40000 ms") && message.contains("20000 ms"))
    );
    assert!(
        provider_recovery_delay(&error, 2, MAX_PROVIDER_RECOVERY_WAIT, DEFAULT_BACKOFF).is_err()
    );
    assert!(!recoverable_provider_error(&ProviderError::Remote {
        message: "http 429: payload text".into()
    }));
}

#[test]
fn retry_after_over_budget_never_sends_an_early_retry() {
    let error = ProviderError::Http {
        status: 429,
        retry_after: Some(std::time::Duration::from_secs(3600)),
        message: "slow down".into(),
    };
    let blocked =
        provider_recovery_delay(&error, 1, std::time::Duration::ZERO, DEFAULT_BACKOFF).unwrap_err();
    assert!(
        matches!(blocked, ProviderError::Http { retry_after: Some(actual), message, .. }
            if actual == std::time::Duration::from_secs(3600)
                && message.contains("3600000 ms") && message.contains("no early retry"))
    );
    let blocked = provider_recovery_delay(&error, 2, MAX_PROVIDER_RECOVERY_WAIT, DEFAULT_BACKOFF)
        .unwrap_err();
    assert!(
        matches!(blocked, ProviderError::Http { status: 429, message, .. } if message.contains("exceeding") && message.contains("work remains pending"))
    );
}

#[test]
fn retry_wait_accepts_exact_budget_and_preserves_structured_metadata() {
    let metadata = Box::new(crate::provider::ProviderErrorMetadata {
        status: Some(503),
        code: Some("server_error".into()),
        error_type: None,
        detail_code: None,
        retry_after: Some(MAX_PROVIDER_RECOVERY_WAIT),
    });
    let error = ProviderError::Api {
        metadata: metadata.clone(),
        message: "busy".into(),
    };
    assert_eq!(
        provider_recovery_delay(&error, 1, std::time::Duration::ZERO, DEFAULT_BACKOFF).unwrap(),
        MAX_PROVIDER_RECOVERY_WAIT
    );
    let blocked = provider_recovery_delay(
        &error,
        2,
        std::time::Duration::from_secs(1),
        DEFAULT_BACKOFF,
    )
    .unwrap_err();
    assert!(
        matches!(blocked, ProviderError::Api { metadata: actual, message }
            if actual == metadata && message.starts_with("busy") && message.contains("work remains pending"))
    );
}

#[test]
fn recoverable_errors_include_timeouts_and_rate_limits_not_auth() {
    assert!(recoverable_provider_error(&ProviderError::Transport {
        safe_to_retry: false,
        message: "provider request timed out before response headers".into(),
    }));
    assert!(recoverable_provider_error(&ProviderError::Transport {
        safe_to_retry: false,
        message: "provider stream idle timeout".into(),
    }));
    assert!(recoverable_provider_error(&ProviderError::Transport {
        safe_to_retry: false,
        message: "provider stream interrupted: connection reset".into(),
    }));
    assert!(recoverable_provider_error(&ProviderError::Http {
        status: 408,
        retry_after: None,
        message: "request timeout".into(),
    }));
    assert!(recoverable_provider_error(&ProviderError::Http {
        status: 429,
        retry_after: None,
        message: "slow down".into(),
    }));
    assert!(!recoverable_provider_error(&ProviderError::Http {
        status: 401,
        retry_after: None,
        message: "unauthorized".into(),
    }));
    assert!(!recoverable_provider_error(&ProviderError::Cancelled));
    assert!(!recoverable_provider_error(
        &ProviderError::MalformedToolCall
    ));
}

#[test]
fn evidence_dedup_requires_the_original_tool_content_in_active_history() {
    let output = "retained evidence ".repeat(20);
    let original = ProviderMessage::tool("read", "original", &output);
    assert!(tool_output_already_in_context(
        std::slice::from_ref(&original),
        "read",
        &output
    ));
    assert!(!tool_output_already_in_context(
        std::slice::from_ref(&original),
        "search",
        &output
    ));
    assert!(!tool_output_already_in_context(
        std::slice::from_ref(&original),
        "read",
        "changed"
    ));
    let pointer = "[duplicate read result omitted; identical output already in context]";
    let messages = vec![
        ProviderMessage::user("read the file"),
        ProviderMessage::assistant(
            "",
            vec![tool_call("original", "read", r#"{"path":"file.txt"}"#)],
        ),
        original,
        ProviderMessage::assistant("done", vec![]),
        ProviderMessage::user("read it again"),
    ];
    let plan = prepare_compaction(
        &messages,
        &CompactionPolicy {
            keep_recent_tokens: 1,
            ..CompactionPolicy::default()
        }
        .settings(),
        ContextUsage::default(),
    )
    .expect("compaction plan");
    assert_eq!(plan.first_kept_index, 4);
    let mut compacted = apply_compaction(&messages, plan.first_kept_index, &output);
    assert!(compacted.iter().all(|message| message.role != "tool"));
    compacted.push(ProviderMessage::tool("read", "pointer", pointer));
    assert!(!tool_output_already_in_context(&compacted, "read", &output));
    assert!(!tool_output_already_in_context(&compacted, "read", pointer));
}

fn tool_call(id: &str, name: &str, arguments: impl Into<String>) -> ProviderToolCall {
    ProviderToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

fn call(id: &str, name: &str, path: &str) -> ProviderToolCall {
    tool_call(id, name, format!(r#"{{"path":"{path}"}}"#))
}

#[test]
fn elision_replaces_evidence_superseded_by_a_later_successful_write() {
    let read_output = "old bytes ".repeat(40);
    let recovery = format!(
            "stale read: a.txt; the precondition differs. No write applied.\nCurrent file is below:\n{}",
            "x".repeat(400)
        );
    let mut messages = vec![
        ProviderMessage::user("fix a"),
        ProviderMessage::assistant("", vec![call("r1", "read", "a.txt")]),
        ProviderMessage::tool("read", "r1", &read_output),
        ProviderMessage::assistant("", vec![call("w1", "write", "a.txt")]),
        ProviderMessage::tool("write", "w1", &recovery),
        ProviderMessage::assistant("", vec![call("w2", "write", "a.txt")]),
        ProviderMessage::tool(
            "write",
            "w2",
            "written a.txt; bytes=5; sha256=abc; exists=true; do not re-read",
        ),
    ];
    let stats = elide_superseded_tool_outputs(&mut messages);
    assert_eq!(stats.elided, 2);
    assert_eq!(
        stats.original_bytes as usize,
        read_output.len() + recovery.len()
    );
    assert_eq!(
        messages[2].content,
        "[superseded read output elided; a.txt was overwritten by a later write]"
    );
    assert_eq!(
        messages[4].content,
        "[superseded write failure output elided; a.txt was updated by a later mutation]"
    );
    assert_eq!(
        messages[6].content,
        "written a.txt; bytes=5; sha256=abc; exists=true; do not re-read"
    );
}

#[test]
fn elision_replaces_shell_output_superseded_by_a_later_run_of_the_same_command() {
    let shell = |id: &str, command: &str| {
        ProviderMessage::assistant(
            "",
            vec![tool_call(
                id,
                "shell",
                format!(r#"{{"command":"{command}"}}"#),
            )],
        )
    };
    let failed = format!(
        "exit 101\nstdout:\n{}stderr:\n",
        "test a ... FAILED\n".repeat(30)
    );
    let other = format!("exit 0\nstdout:\n{}stderr:\n", "M file\n".repeat(30));
    let timed_out = format!(
        "exit n/a · timed out\nstdout:\n{}stderr:\n",
        "x".repeat(300)
    );
    let passed = "exit 0\nstdout:\ntest result: ok\nstderr:\n";
    let mut messages = vec![
        ProviderMessage::user("fix a"),
        shell("s1", "cargo test"),
        ProviderMessage::tool("shell", "s1", &failed),
        shell("s2", "git status"),
        ProviderMessage::tool("shell", "s2", &other),
        shell("s3", "cargo test"),
        ProviderMessage::tool("shell", "s3", passed),
        shell("s4", "git status"),
        ProviderMessage::tool("shell", "s4", &timed_out),
    ];
    let stats = elide_superseded_tool_outputs(&mut messages);
    assert_eq!(stats.elided, 1);
    assert_eq!(
        messages[2].content,
        "[superseded shell output elided; exit 101; the same command ran again later]"
    );
    assert_eq!(
        messages[2].recorded_content.as_deref(),
        Some(failed.as_str())
    );
    // A different command, the latest run, and a run superseded only by an
    // unfinished one all stay.
    assert_eq!(messages[4].content, other);
    assert_eq!(messages[6].content, passed);
    assert_eq!(messages[8].content, timed_out);
}

#[test]
fn shell_elision_keeps_runs_a_later_pointer_refers_to_and_ignores_runs_without_an_exit_code() {
    let shell = |id: &str| {
        ProviderMessage::assistant(
            "",
            vec![tool_call(id, "shell", r#"{"command":"cargo test"}"#)],
        )
    };
    let failed = format!("exit 101\nstdout:\n{}stderr:\n", "FAILED\n".repeat(40));
    let passed = format!("exit 0\nstdout:\n{}stderr:\n", "ok\n".repeat(40));
    let pointer = duplicate_pointer("shell");
    // A later duplicate pointer stands for one of the earlier runs: they stay.
    let mut messages = vec![
        shell("s1"),
        ProviderMessage::tool("shell", "s1", &failed),
        shell("s2"),
        ProviderMessage::tool("shell", "s2", &passed),
        shell("s3"),
        ProviderMessage::tool("shell", "s3", &pointer),
    ];
    assert_eq!(elide_superseded_tool_outputs(&mut messages).elided, 0);
    assert_eq!(messages[1].content, failed);
    // A later run without an exit code is not a completed run.
    let no_code = format!("exit n/a\nstdout:\n{}stderr:\n", "x".repeat(300));
    let mut messages = vec![
        shell("s1"),
        ProviderMessage::tool("shell", "s1", &failed),
        shell("s2"),
        ProviderMessage::tool("shell", "s2", &no_code),
    ];
    assert_eq!(elide_superseded_tool_outputs(&mut messages).elided, 0);
    // The elided message is a pointer like the others: it carries the prefix
    // that compaction recognises.
    let mut messages = vec![
        shell("s1"),
        ProviderMessage::tool("shell", "s1", &failed),
        shell("s2"),
        ProviderMessage::tool("shell", "s2", &passed),
    ];
    assert_eq!(elide_superseded_tool_outputs(&mut messages).elided, 1);
    assert!(messages[1].content.starts_with("[superseded "));
    assert_eq!(elide_superseded_tool_outputs(&mut messages).elided, 0);
}

#[test]
fn elision_keeps_reads_after_patch_but_elides_the_superseded_recovery_body() {
    let read_output = "still current except the edited span ".repeat(20);
    let patch_recovery = format!(
        "a.txt: file unchanged. Matches start at lines 2.\nCurrent file is below:\n{}",
        "y".repeat(400)
    );
    let mut messages = vec![
        ProviderMessage::user("fix a"),
        ProviderMessage::assistant("", vec![call("r1", "read", "a.txt")]),
        ProviderMessage::tool("read", "r1", &read_output),
        ProviderMessage::assistant("", vec![call("p1", "patch", "a.txt")]),
        ProviderMessage::tool("patch", "p1", &patch_recovery),
        ProviderMessage::assistant("", vec![call("p2", "patch", "a.txt")]),
        ProviderMessage::tool(
            "patch",
            "p2",
            "patched a.txt; 1 edits applied atomically; bytes=9; sha256=def; do not re-read",
        ),
    ];
    let stats = elide_superseded_tool_outputs(&mut messages);
    // Partial staleness is still evidence: the read survives a patch.
    assert_eq!(stats.elided, 1);
    assert_eq!(messages[2].content, read_output);
    assert_eq!(
        messages[4].content,
        "[superseded patch failure output elided; a.txt was updated by a later mutation]"
    );
}

#[test]
fn elision_recognizes_current_and_legacy_ambiguous_patch_context() {
    for marker in [
            "Suggested unique expected:",
            "Example context only for the first match at line 2; choose the intended occurrence explicitly:",
        ] {
            let recovery = format!("file unchanged.\n{marker}\n{}", "context\n".repeat(80));
            let mut messages = vec![
                ProviderMessage::assistant("", vec![call("p1", "patch", "a.txt")]),
                ProviderMessage::tool("patch", "p1", &recovery),
                ProviderMessage::assistant("", vec![call("p2", "patch", "a.txt")]),
                ProviderMessage::tool(
                    "patch", "p2",
                    "patched a.txt; 1 edits applied atomically; bytes=9; sha256=def; do not re-read",
                ),
            ];
            assert_eq!(elide_superseded_tool_outputs(&mut messages).elided, 1);
            assert!(messages[1].content.starts_with("[superseded patch failure output elided;"));
            assert!(messages[3].content.starts_with("patched a.txt;"));
        }
}

#[test]
fn elision_keeps_the_read_taken_after_the_last_write() {
    let stale = "first version ".repeat(30);
    let current = "second version ".repeat(30);
    let mut messages = vec![
        ProviderMessage::assistant("", vec![call("r1", "read", "a.txt")]),
        ProviderMessage::tool("read", "r1", &stale),
        ProviderMessage::assistant("", vec![call("w1", "write", "a.txt")]),
        ProviderMessage::tool(
            "write",
            "w1",
            "written a.txt; bytes=9; sha256=abc; exists=true; do not re-read",
        ),
        ProviderMessage::assistant("", vec![call("r2", "read", "a.txt")]),
        ProviderMessage::tool("read", "r2", &current),
    ];
    let stats = elide_superseded_tool_outputs(&mut messages);
    assert_eq!(stats.elided, 1);
    assert!(messages[1].content.starts_with("[superseded read"));
    assert_eq!(messages[5].content, current);
}

#[test]
fn elision_scopes_recycled_call_ids_to_their_own_assistant_turn() {
    let first = "a version ".repeat(40);
    let second = "b version ".repeat(40);
    let mut messages = vec![
        ProviderMessage::user("fix b"),
        ProviderMessage::assistant("", vec![call("0", "read", "a.txt")]),
        ProviderMessage::tool("read", "0", &first),
        ProviderMessage::assistant("", vec![call("0", "read", "b.txt")]),
        ProviderMessage::tool("read", "0", &second),
        ProviderMessage::assistant("", vec![call("0", "write", "b.txt")]),
        ProviderMessage::tool(
            "write",
            "0",
            "written b.txt; bytes=9; sha256=abc; exists=true; do not re-read",
        ),
    ];
    let stats = elide_superseded_tool_outputs(&mut messages);
    assert_eq!(stats.elided, 1);
    assert_eq!(messages[2].content, first, "a.txt was never written");
    assert_eq!(
        messages[4].content,
        "[superseded read output elided; b.txt was overwritten by a later write]"
    );
}

#[test]
fn tool_call_path_reads_the_same_path_as_a_full_parse() {
    fn reference(arguments: &str) -> Option<(String, String)> {
        let raw = serde_json::from_str::<Value>(arguments)
            .ok()?
            .get("path")?
            .as_str()?
            .to_owned();
        let key = crate::tools::path_identity(Path::new(&raw));
        (!key.is_empty()).then_some((key, raw))
    }
    let big = "x".repeat(4096);
    let cases = [
        r#"{"path":"a.txt"}"#.to_owned(),
        r#"{"path":"dir\\sub/a.txt","content":"c"}"#.to_owned(),
        format!(r#"{{"content":"{big}\nç\"","path":"src\\lib.rs","n":[1,{{"path":"no"}}]}}"#),
        r#"{"path":"escaped-key.txt"}"#.to_owned(),
        r#"{"path":"first","path":"second"}"#.to_owned(),
        r#"{"path":"first","path":7}"#.to_owned(),
        r#"{"path":7}"#.to_owned(),
        r#"{"path":null,"x":true}"#.to_owned(),
        r#"{"path":["a"]}"#.to_owned(),
        r#"{"path":{"path":"nested"}}"#.to_owned(),
        r#"{"path":""}"#.to_owned(),
        r#"{"Path":"case.txt"}"#.to_owned(),
        r#"{"a":1.5e3,"b":-2,"c":18446744073709551616,"path":"n.txt"}"#.to_owned(),
        r#"{}"#.to_owned(),
        r#"[]"#.to_owned(),
        r#"["a.txt"]"#.to_owned(),
        r#""a.txt""#.to_owned(),
        r#"null"#.to_owned(),
        String::new(),
        r#"{"path":"a.txt""#.to_owned(),
        r#"{"path":"a.txt"} trailing"#.to_owned(),
        r#"{"path":"a.txt",}"#.to_owned(),
        r#"{"path":"bad \q escape"}"#.to_owned(),
    ];
    for arguments in &cases {
        assert_eq!(
            super::history_elision::tool_call_path(arguments),
            reference(arguments),
            "{arguments}"
        );
    }
}

#[test]
fn elision_ignores_other_paths_failed_mutations_and_reruns_idempotently() {
    let output = "content ".repeat(50);
    let mut messages = vec![
        ProviderMessage::assistant("", vec![call("r1", "read", "a.txt")]),
        ProviderMessage::tool("read", "r1", &output),
        ProviderMessage::assistant("", vec![call("w1", "write", "b.txt")]),
        ProviderMessage::tool(
            "write",
            "w1",
            "written b.txt; bytes=1; sha256=x; exists=true; do not re-read",
        ),
        ProviderMessage::assistant("", vec![call("w2", "write", "c.txt")]),
        ProviderMessage::tool("write", "w2", "stale read: c.txt; no write applied"),
        // No path in the arguments: nothing to key on, never elides.
        ProviderMessage::assistant("", vec![tool_call("r2", "read", "invalid")]),
        ProviderMessage::tool("read", "r2", &output),
    ];
    let stats = elide_superseded_tool_outputs(&mut messages);
    assert_eq!(stats, ElisionStats::default());
    assert_eq!(messages[1].content, output);
    // The failed write carries no recovery body and no later mutation.
    assert_eq!(messages[5].content, "stale read: c.txt; no write applied");
    assert_eq!(messages[7].content, output);
    assert_eq!(
        elide_superseded_tool_outputs(&mut messages),
        ElisionStats::default()
    );
}

#[test]
fn an_elided_output_keeps_the_compaction_fingerprint_of_what_the_journal_recorded() {
    let output = "old bytes ".repeat(40);
    let mut messages = vec![
        ProviderMessage::user("fix a"),
        ProviderMessage::assistant("", vec![call("r1", "read", "a.txt")]),
        ProviderMessage::tool("read", "r1", &output),
        ProviderMessage::assistant(
            "",
            vec![tool_call(
                "w1",
                "write",
                r#"{"path":"a.txt","content":"new"}"#,
            )],
        ),
        ProviderMessage::tool(
            "write",
            "w1",
            "written a.txt; bytes=3; sha256=x; exists=true",
        ),
        ProviderMessage::assistant("kept", Vec::new()),
    ];
    let recorded = compaction_prefix_fingerprint(&messages[..5]);
    let stats = elide_superseded_tool_outputs(&mut messages);
    assert_eq!(stats.elided, 1);
    assert!(messages[2]
        .content
        .starts_with("[superseded read output elided;"));
    // The live message changed, the journal's did not: the fingerprints agree.
    assert_eq!(compaction_prefix_fingerprint(&messages[..5]), recorded);
    // Eliding again keeps the first recorded form.
    elide_superseded_tool_outputs(&mut messages);
    assert_eq!(compaction_prefix_fingerprint(&messages[..5]), recorded);
    // A different output is a different prefix.
    messages[2].recorded_content = Some(Arc::from("something else"));
    assert_ne!(compaction_prefix_fingerprint(&messages[..5]), recorded);
}

#[test]
fn truncate_result_marker_stays_within_the_byte_limit() {
    assert_eq!(truncate_result("short", 50), "short");
    for (input, limit) in [("x".repeat(100), 50), ("日本語".repeat(20), 50)] {
        let truncated = truncate_result(&input, limit);
        assert!(truncated.len() <= limit, "{} > {limit}", truncated.len());
        assert!(truncated.ends_with("\n[truncated]"));
        assert!(input.starts_with(truncated.trim_end_matches("\n[truncated]")));
    }
    // Too small for the marker: a plain cut still respects the limit.
    assert_eq!(truncate_result(&"x".repeat(100), 5), "xxxxx");
}

#[test]
fn redaction_ignores_empty_secrets() {
    let secrets = vec![String::new(), "secret".to_owned()];
    assert_eq!(redact_values(&secrets, "abc"), "abc");
    assert_eq!(
        redact_values(&secrets, "a secret b"),
        "a [REDACTED] b",
        "an empty value must not be spliced between characters"
    );
}

#[test]
fn tool_progress_announcement_redacts_the_tool_name() {
    let mut normalizer =
        ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec!["sk-secret".into()]);
    let mut app = AppHandle::fake();
    normalizer.push(
        &mut app,
        ProviderEvent::ToolCallProgress {
            name: Some("sk-secret".into()),
            bytes: 10,
        },
    );

    let details: Vec<_> = app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ProviderPhase {
                phase: ProviderPhase::PreparingTool,
                detail,
                ..
            } => detail.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(details, ["[REDACTED] · 10 B"]);
}

#[test]
fn codex_completion_rejects_conflicting_identity_or_arguments() {
    for (index, id, name, arguments) in [
        (0, "other-id", "read", r#"{"path":"a"}"#),
        (1, "call-a", "read", r#"{"path":"a"}"#),
        (0, "call-a", "write", r#"{"path":"a"}"#),
        (0, "call-a", "read", r#"{"path":"b"}"#),
        (0, "call-a", "read", "invalid JSON"),
    ] {
        let mut normalizer = ProviderStreamNormalizer::new(ProviderKind::OpenAiCodex, 1, vec![]);
        let mut app = AppHandle::fake();
        normalizer.push(
            &mut app,
            ProviderEvent::ToolCallDelta {
                index: Some(0),
                id: Some("call-a".into()),
                name: Some("read".into()),
                arguments: r#"{"path":"a"}"#.into(),
            },
        );
        normalizer.push(
            &mut app,
            ProviderEvent::ToolCallComplete {
                index,
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
            },
        );
        normalizer.push(
            &mut app,
            ProviderEvent::Stopped {
                reason: "completed".into(),
            },
        );
        assert!(matches!(
            normalizer.finish(&mut app),
            Err(ProviderError::MalformedToolCall)
        ));
        assert!(!app
            .events()
            .iter()
            .any(|event| matches!(event.kind, crate::EventKind::ProviderToolCall { .. })));
    }
}

#[derive(Default)]
struct FixtureCodeIntel {
    fail: bool,
    failure_reason: Option<&'static str>,
    rejects_workspace: bool,
    scoped_queries: std::sync::Mutex<Vec<Option<std::path::PathBuf>>>,
    updates: std::sync::Mutex<Vec<crate::codeintel::CodeIntelFileUpdate>>,
    changes: std::sync::Mutex<Vec<Option<String>>>,
    sync_blocked: bool,
    sync_started: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl crate::codeintel::CodeIntelligence for FixtureCodeIntel {
    fn supports_workspace(&self, _workspace: &Path) -> bool {
        !self.rejects_workspace
    }

    async fn status(&self, _workspace: &Path) -> crate::codeintel::CodeIntelOutcome {
        if self.fail {
            return crate::codeintel::CodeIntelOutcome::unavailable(
                "fixture",
                self.failure_reason.unwrap_or("request timed out"),
            );
        }
        crate::codeintel::CodeIntelOutcome {
            meta: crate::codeintel::CodeIntelMeta {
                server: "fixture".into(),
                state: crate::codeintel::CodeIntelServerState::Ready,
                completeness: crate::codeintel::CodeIntelCompleteness::Complete,
                document_version: None,
                stale: false,
                elapsed_ms: 0,
            },
            payload: json!({ "servers": [], "summary": "ok" }),
        }
    }

    async fn definition(
        &self,
        _query: &crate::codeintel::CodeIntelPositionQuery,
    ) -> crate::codeintel::CodeIntelOutcome {
        if self.fail {
            return crate::codeintel::CodeIntelOutcome::unavailable(
                "fixture",
                self.failure_reason.unwrap_or("request timed out"),
            );
        }
        crate::codeintel::CodeIntelOutcome {
            meta: crate::codeintel::CodeIntelMeta {
                server: "fixture".into(),
                state: crate::codeintel::CodeIntelServerState::Ready,
                completeness: crate::codeintel::CodeIntelCompleteness::Complete,
                document_version: Some(1),
                stale: false,
                elapsed_ms: 0,
            },
            payload: json!({ "found": false, "file": "", "line": 0, "column": 0 }),
        }
    }

    async fn references(
        &self,
        _query: &crate::codeintel::CodeIntelPositionQuery,
    ) -> crate::codeintel::CodeIntelOutcome {
        crate::codeintel::CodeIntelOutcome::unavailable("fixture", "unused")
    }

    async fn hover(
        &self,
        _query: &crate::codeintel::CodeIntelPositionQuery,
    ) -> crate::codeintel::CodeIntelOutcome {
        crate::codeintel::CodeIntelOutcome::unavailable("fixture", "unused")
    }

    async fn symbols(
        &self,
        query: &crate::codeintel::CodeIntelSymbolQuery,
    ) -> crate::codeintel::CodeIntelOutcome {
        self.scoped_queries.lock().unwrap().push(query.path.clone());
        crate::codeintel::CodeIntelOutcome::unavailable("fixture", "unused")
    }

    async fn diagnostics(
        &self,
        query: &crate::codeintel::CodeIntelDiagnosticsQuery,
    ) -> crate::codeintel::CodeIntelOutcome {
        self.scoped_queries.lock().unwrap().push(query.path.clone());
        crate::codeintel::CodeIntelOutcome::unavailable("fixture", "unused")
    }

    async fn notify_file_changed(&self, _workspace: &Path, _path: &Path, text: Option<String>) {
        self.changes.lock().unwrap().push(text);
    }
    async fn notify_file_updated(
        &self,
        _workspace: &Path,
        _path: &Path,
        update: crate::codeintel::CodeIntelFileUpdate,
    ) {
        self.updates.lock().unwrap().push(update);
        if self.sync_blocked {
            self.sync_started.notify_one();
            std::future::pending::<()>().await;
        }
    }
}

#[tokio::test]
async fn native_patch_passes_sequential_ranges_through_runtime() {
    let root = TempRoot::new("runtime-patch-sync");
    let original = "fn \u{e9}\u{1f680}e\u{301}first() {}\r\nfn second() {}\r\n";
    std::fs::write(root.join("main.rs"), original).unwrap();
    let backend = Arc::new(FixtureCodeIntel::default());
    let mut runtime = Runtime::new();
    runtime.set_code_intelligence(backend.clone());
    let calls = vec![provider_call(
        "patch",
        "patch",
        json!({"path":"main.rs", "edits":[
            {"expected":"first", "replacement":"first_longer() {}\nfn inserted"},
            {"expected":"second", "replacement":"updated"}
        ]}),
    )];
    let (results, _) = runtime
        .execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "batch",
            &calls,
            1,
            &mut CausalGovernor::default(),
        )
        .await
        .unwrap();
    assert!(results[0].success, "{}", results[0].output);
    let updates = backend.updates.lock().unwrap();
    assert_eq!(updates.len(), 1);
    let update = &updates[0];
    assert_eq!(
        update.text,
        std::fs::read_to_string(root.join("main.rs")).unwrap()
    );
    let patch = update.patch.as_ref().unwrap();
    assert!(patch.matches_before(original));
    assert_eq!(patch.edits.len(), 2);
    assert_eq!(patch.edits[0].start.line, 0);
    assert_eq!(patch.edits[0].start.prefix, "fn \u{e9}\u{1f680}e\u{301}");
    assert_eq!(patch.edits[0].end.prefix, "fn \u{e9}\u{1f680}e\u{301}first");
    assert_eq!(patch.edits[0].text, "first_longer() {}\r\nfn inserted");
    assert_eq!(
        patch.edits[1].start.line, 2,
        "second range uses the new line layout"
    );
    assert_eq!(patch.edits[1].start.prefix, "fn ");
    drop(updates);
    drop(runtime);
}

#[cfg(windows)]
#[tokio::test]
async fn fused_shell_syncs_the_file_after_the_shell_runs() {
    let root = TempRoot::new("fused-sync");
    std::fs::write(root.join("source.rs"), "before").unwrap();
    let backend = Arc::new(FixtureCodeIntel::default());
    let mut runtime = Runtime::new();
    runtime.set_code_intelligence(backend.clone());
    let calls = vec![provider_call(
        "write",
        "write",
        json!({
            "path": "source.rs",
            "content": "edited",
            "expected": "before",
            "then_run": {"command": "Set-Content -LiteralPath source.rs -Value shell"}
        }),
    )];
    let (results, _) = runtime
        .execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "batch",
            &calls,
            1,
            &mut CausalGovernor::default(),
        )
        .await
        .unwrap();
    assert!(results[0].success, "{}", results[0].output);
    assert_eq!(
        std::fs::read_to_string(root.join("source.rs"))
            .unwrap()
            .trim(),
        "shell"
    );
    assert!(backend.updates.lock().unwrap().is_empty());
    assert_eq!(backend.changes.lock().unwrap().as_slice(), &[None]);
    drop(runtime);
}

#[tokio::test]
async fn cancellation_interrupts_post_patch_synchronization() {
    let root = TempRoot::new("runtime-sync-cancel");
    std::fs::write(root.join("main.rs"), "old").unwrap();
    let backend = Arc::new(FixtureCodeIntel {
        sync_blocked: true,
        ..Default::default()
    });
    let cancellation = CancellationToken::new();
    let mut runtime = Runtime::new();
    runtime.set_code_intelligence(backend.clone());
    runtime.set_cancellation_token(cancellation.clone());
    let calls = vec![provider_call(
        "patch",
        "patch",
        json!({"path":"main.rs", "edits":[{"expected":"old", "replacement":"new"}]}),
    )];
    let mut governor = CausalGovernor::default();
    let execute = runtime.execute_provider_tool_batch(
        crate::OperatingMode::Auto,
        &root,
        "batch",
        &calls,
        1,
        &mut governor,
    );
    tokio::pin!(execute);
    tokio::select! {
        _ = &mut execute => panic!("sync should be blocked"),
        _ = backend.sync_started.notified() => {},
    }
    cancellation.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), execute)
        .await
        .expect("cancel unblocks runtime sync");
    assert_eq!(
        std::fs::read_to_string(root.join("main.rs")).unwrap(),
        "new"
    );
}

#[tokio::test]
async fn code_intel_error_payload_reports_failure() {
    let request = Ok(CodeIntelRequest::Status {
        workspace: std::path::PathBuf::from("D:/demo"),
    });
    let (result, _) = run_code_intel_request(
        Some(Arc::new(FixtureCodeIntel {
            fail: true,
            ..Default::default()
        })),
        request,
        None,
    )
    .await;
    assert!(!result.success);
    assert!(result.output.contains("request timed out"));
}

#[tokio::test]
async fn code_intel_completed_negative_answer_stays_successful() {
    let request = Ok(CodeIntelRequest::Definition(
        crate::codeintel::CodeIntelPositionQuery {
            workspace: std::path::PathBuf::from("D:/demo"),
            path: std::path::PathBuf::from("D:/demo/main.rs"),
            line: 1,
            column: 1,
            symbol: None,
            max_results: 20,
            offset: 0,
            revision: None,
            cancellation: None,
        },
    ));
    let (result, _) =
        run_code_intel_request(Some(Arc::new(FixtureCodeIntel::default())), request, None).await;
    assert!(result.success);
    assert!(result.output.contains("not found"));
}

#[tokio::test]
async fn code_intel_invalid_path_never_reaches_backend_and_valid_scope_is_preserved() {
    let root = TempRoot::new("code-intel-path-types");
    let path = root.join("ação com espaço.rs");
    std::fs::write(&path, "fn target() {}\n").unwrap();
    let backend = Arc::new(FixtureCodeIntel::default());
    let mut runtime = Runtime::new();
    runtime.set_code_intelligence(backend.clone());
    let mut calls = Vec::new();
    for action in ["symbol", "diagnostics"] {
        for (index, invalid) in [json!(42), json!(true), json!([]), json!({})]
            .into_iter()
            .enumerate()
        {
            calls.push(provider_call(
                &format!("{action}-{index}"),
                "code_intel",
                json!({"action":action,"path":invalid,"query":"target"}),
            ));
        }
    }
    let (results, next) = runtime
        .execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "invalid-paths",
            &calls,
            1,
            &mut CausalGovernor::default(),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), calls.len());
    assert!(results
        .iter()
        .all(|result| !result.success && result.output.contains("path must be a string")));
    assert!(backend.scoped_queries.lock().unwrap().is_empty());

    let mut valid_calls = Vec::new();
    for action in ["symbol", "diagnostics"] {
        valid_calls.push(provider_call(
            &format!("valid-{action}"),
            "code_intel",
            json!({"action":action,"path":"ação com espaço.rs","query":"target"}),
        ));
    }
    runtime
        .execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "valid-paths",
            &valid_calls,
            next,
            &mut CausalGovernor::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        *backend.scoped_queries.lock().unwrap(),
        vec![Some(std::fs::canonicalize(&path).unwrap()); 2]
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn target() {}\n");
}

#[tokio::test]
async fn code_intel_startup_diagnostic_is_redacted_in_results_and_events() {
    let root = TempRoot::new("code-intel-stderr-redaction");
    std::fs::write(root.join("main.rs"), "fn target() {}\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.register_sensitive_value("private-lsp-token");
    runtime.set_code_intelligence(Arc::new(FixtureCodeIntel {
        fail: true,
        failure_reason: Some(
            "initialization failed: eof\nserver stderr:\nsysroot unavailable: private-lsp-token",
        ),
        ..Default::default()
    }));
    let calls = vec![provider_call(
        "code-intel",
        "code_intel",
        json!({
            "action":"definition", "path":"main.rs", "line":1, "column":4,
        }),
    )];
    let (results, _) = runtime
        .execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "startup-diagnostic",
            &calls,
            1,
            &mut CausalGovernor::default(),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(!results[0].success);
    assert!(results[0].output.contains("sysroot unavailable"));
    assert!(results[0].output.contains("[REDACTED]"));
    assert!(!results[0].output.contains("private-lsp-token"));
    let event_output = runtime
        .app
        .events()
        .iter()
        .find_map(|event| match &event.kind {
            crate::EventKind::ToolOutput { output, .. } => Some(output),
            _ => None,
        })
        .expect("diagnostic event");
    assert!(event_output.contains("sysroot unavailable"));
    assert!(event_output.contains("[REDACTED]"));
    assert!(!serde_json::to_string(runtime.app.events())
        .unwrap()
        .contains("private-lsp-token"));
    drop(runtime);
}

const SEQUENTIAL_LSP_SECRET: &str = "sequential-lsp-secret";

/// Runs a `code_intel` call whose backend echoes the secret in its diagnostic,
/// straight through the sequential executor (not the batch scheduler).
async fn sequential_code_intel_with_secret(
    runtime: &mut Runtime,
    root: &std::path::Path,
) -> ToolResult {
    runtime.register_sensitive_value(SEQUENTIAL_LSP_SECRET);
    runtime.set_code_intelligence(Arc::new(FixtureCodeIntel {
        fail: true,
        failure_reason: Some(
            "initialization failed: eof\nserver stderr:\nsysroot unavailable: sequential-lsp-secret",
        ),
        ..Default::default()
    }));
    let arguments = json!({
        "action":"definition", "path":"main.rs", "line":1, "column":4,
    })
    .to_string();
    let prepared = runtime.tools.prepare_invocation(
        crate::OperatingMode::Auto,
        root,
        "code_intel",
        &arguments,
    );
    let (result, _) = runtime
        .execute_code_intel(
            ToolInvocation {
                batch_id: "sequential-redaction",
                call_id: "code-intel",
                name: "code_intel",
                arguments: &arguments,
            },
            &prepared,
            1,
        )
        .await
        .unwrap();
    result
}

#[tokio::test]
async fn sequential_code_intel_returns_redacted_output_and_matching_event() {
    let root = TempRoot::new("code-intel-sequential-redaction");
    std::fs::write(root.join("main.rs"), "fn target() {}\n").unwrap();
    let mut runtime = Runtime::new();
    let result = sequential_code_intel_with_secret(&mut runtime, &root).await;
    assert!(!result.success);
    assert!(result.output.contains("sysroot unavailable"));
    assert!(result.output.contains("[REDACTED]"));
    assert!(!result.output.contains(SEQUENTIAL_LSP_SECRET));
    let event_output = runtime
        .app
        .events()
        .iter()
        .find_map(|event| match &event.kind {
            crate::EventKind::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("ToolOutput event");
    assert_eq!(event_output, result.output);
    assert!(!serde_json::to_string(runtime.app.events())
        .unwrap()
        .contains(SEQUENTIAL_LSP_SECRET));
}

#[tokio::test]
async fn sequential_result_redacted_before_it_reaches_the_artifact_store() {
    let root = TempRoot::new("code-intel-artifact-redaction");
    std::fs::write(root.join("main.rs"), "fn target() {}\n").unwrap();
    let mut runtime = Runtime::with_artifact_store(root.join(".slim/artifacts")).unwrap();
    let result = sequential_code_intel_with_secret(&mut runtime, &root).await;
    let mut results = [result];
    // A one-byte cap forces the redacted output into the artifact store.
    runtime
        .materialize_results(&mut results, 1, None, 100)
        .await
        .unwrap();
    let handle = results[0].artifact.clone().expect("output stored");
    let stored = std::fs::read_to_string(&handle.path).unwrap();
    assert!(stored.contains("sysroot unavailable"));
    assert!(stored.contains("[REDACTED]"));
    assert!(!stored.contains(SEQUENTIAL_LSP_SECRET));
    assert!(!results[0].output.contains(SEQUENTIAL_LSP_SECRET));
}

#[test]
fn todo_tool_returns_redacted_output_and_matching_event() {
    let cwd = std::env::temp_dir();
    let mut runtime = Runtime::new();
    runtime.register_sensitive_value("todo-secret-value");
    runtime.prepare_loop_capabilities(&cwd).unwrap();
    let (result, _) = runtime
        .execute_todo(
            crate::OperatingMode::Auto,
            ToolInvocation {
                batch_id: "todo-redaction",
                call_id: "create",
                name: "todo",
                arguments: &json!({"todos":[
                    {"title":"rotate todo-secret-value", "status":"pending"}
                ]})
                .to_string(),
            },
            1,
        )
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("[REDACTED]"), "{}", result.output);
    assert!(!result.output.contains("todo-secret-value"));
    let event_output = runtime
        .app
        .events()
        .iter()
        .find_map(|event| match &event.kind {
            crate::EventKind::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("ToolOutput event");
    assert_eq!(event_output, result.output);
}

#[tokio::test]
async fn code_intel_batch_preserves_admission_feedback_in_output_event() {
    let root = TempRoot::new("code-intel-admission");
    std::fs::write(root.join("main.rs"), "fn target() {}\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.set_code_intelligence(Arc::new(FixtureCodeIntel::default()));
    let calls = vec![provider_call(
        "code-intel",
        "code_intel",
        serde_json::json!({
            "action":"definition",
            "path":"main.rs",
            "line":1,
            "column":4,
            "max_results":0
        }),
    )];
    let prepared = runtime
        .prepare_provider_tool_invocations(crate::OperatingMode::Auto, &root, &calls)
        .await
        .expect("code_intel preparation");
    let prefix = crate::tools::admission_output_prefix(&prepared[0].admission_notes)
        .expect("saturated max_results should be visible");
    assert!(prefix.contains("code_intel max_results 0 -> 1"));
    let (results, _) = runtime
        .execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "code-intel-batch",
            &calls,
            1,
            &mut CausalGovernor::default(),
        )
        .await
        .expect("code_intel batch");
    assert_eq!(results.len(), 1);
    assert!(
        results[0].output.starts_with(&prefix),
        "{}",
        results[0].output
    );
    let event_output = runtime
        .app
        .events()
        .iter()
        .find_map(|event| match &event.kind {
            crate::EventKind::ToolOutput {
                call_id, output, ..
            } if call_id == "code-intel" => Some(output),
            _ => None,
        });
    assert_eq!(event_output, Some(&results[0].output));
}

#[test]
fn native_shell_process_facts_keep_nonterminating_error_boundary() {
    let root = TempRoot::new("shell-process-facts");
    let mut runtime = Runtime::new();
    let (result, _) = runtime
        .execute_tool(
            crate::OperatingMode::Auto,
            &root,
            "shell",
            r#"{"command":"Write-Error 'SLIM_TEST_NONTERMINATING'; Write-Output 'continued'"}"#,
            1,
        )
        .expect("native shell execution");
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("continued"), "{}", result.output);

    let process = runtime
        .app
        .events()
        .iter()
        .find_map(|event| match &event.kind {
            crate::EventKind::ToolProcessFinished { process, .. } => Some(process),
            _ => None,
        });
    let process = process.expect("native shell must publish process facts");
    assert_eq!(process.exit_code, Some(0));
    assert!(process.stdout_bytes > 0, "{process:?}");
    assert!(process.stderr_bytes > 0, "{process:?}");
    assert!(!process.timed_out, "{process:?}");
    assert!(!process.cancelled, "{process:?}");
}

#[test]
fn tool_definition_sets_are_shared_per_semantic_key() {
    let cwd = std::env::temp_dir();
    let runtime = Runtime::new();
    let first = runtime.workspace_tool_definitions(crate::OperatingMode::Auto, &cwd);
    let second = runtime.workspace_tool_definitions(crate::OperatingMode::Auto, &cwd);
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(
        runtime.advertised_tool_definitions(crate::OperatingMode::Auto),
        first.as_ref()
    );

    let read_only = runtime.workspace_tool_definitions(crate::OperatingMode::ReadOnly, &cwd);
    assert!(!Arc::ptr_eq(&first, &read_only));
    assert!(Arc::ptr_eq(
        &read_only,
        &runtime.workspace_tool_definitions(crate::OperatingMode::ReadOnly, &cwd)
    ));

    let mut with_intel = Runtime::new();
    with_intel.set_code_intelligence(Arc::new(FixtureCodeIntel::default()));
    let enabled = with_intel.workspace_tool_definitions(crate::OperatingMode::Auto, &cwd);
    assert!(!Arc::ptr_eq(&first, &enabled));
    assert!(enabled.iter().any(|tool| tool["name"] == "code_intel"));

    with_intel.set_code_intelligence(Arc::new(FixtureCodeIntel {
        rejects_workspace: true,
        ..Default::default()
    }));
    let rejected = with_intel.workspace_tool_definitions(crate::OperatingMode::Auto, &cwd);
    assert!(Arc::ptr_eq(&first, &rejected));
    assert!(!rejected.iter().any(|tool| tool["name"] == "code_intel"));

    let mut interactive = Runtime::new();
    let (route, _responder) = crate::interaction_route();
    interactive.set_interaction_route(route);
    let asked = interactive.workspace_tool_definitions(crate::OperatingMode::Auto, &cwd);
    assert!(!Arc::ptr_eq(&first, &asked));
    assert!(asked.iter().any(|tool| tool["name"] == "ask_question"));
    assert!(Arc::ptr_eq(
        &asked,
        &interactive.workspace_tool_definitions(crate::OperatingMode::Auto, &cwd)
    ));
    let interactive_readonly =
        interactive.workspace_tool_definitions(crate::OperatingMode::ReadOnly, &cwd);
    assert!(interactive_readonly
        .iter()
        .any(|tool| tool["name"] == "ask_question"));
    assert!(!Arc::ptr_eq(&interactive_readonly, &read_only));
    let plan = interactive.workspace_tool_definitions(crate::OperatingMode::Plan, &cwd);
    assert!(Arc::ptr_eq(
        &plan,
        &runtime.workspace_tool_definitions(crate::OperatingMode::Plan, &cwd),
    ));
}

#[test]
fn todo_reviews_are_bounded_and_do_not_invent_statuses() {
    let mut cadence = TodoCadence::default();
    let mut items = vec![crate::TodoChangedItem {
        reason: None,
        id: Some(7),
        title: "verify".into(),
        status: "in_progress".into(),
    }];
    // First reminder rides the already-needed turn immediately after task creation.
    assert!(cadence.after_batch(&items));
    for _ in 0..3 {
        assert!(!cadence.after_batch(&items));
    }
    assert!(cadence.after_batch(&items));
    for _ in 0..20 {
        assert!(!cadence.after_batch(&items));
    }
    assert!(cadence.before_final(&items));
    assert!(!cadence.before_final(&items));
    assert_eq!(items[0].status, "in_progress");
    for status in ["blocked", "completed", "cancelled"] {
        items[0].status = status.into();
        let mut fresh = TodoCadence::default();
        assert!(!fresh.before_final(&items));
        for _ in 0..5 {
            assert!(!fresh.after_batch(&items));
        }
    }
    let legacy: crate::TodoChangedItem = serde_json::from_value(json!({
        "title": "old log", "status": "pending"
    }))
    .unwrap();
    assert_eq!(legacy.id, None);
}

#[test]
fn a_batch_of_results_shares_one_presentation_budget() {
    let runtime = Runtime::new();
    let config = AgentLoopConfig::default();
    let present = |count: usize| {
        let calls: Vec<ProviderToolCall> = (0..count)
            .map(|index| ProviderToolCall {
                id: format!("c{index}"),
                name: "shell".into(),
                arguments: "{}".into(),
            })
            .collect();
        // Each result is at its own 16 KiB cap.
        let results: Vec<ToolResult> = (0..count)
            .map(|index| {
                ToolResult::ok(
                    "shell",
                    format!("{index}{}", "x".repeat(config.max_result_bytes - 1)),
                )
            })
            .collect();
        let batch = presentation::PresentationBatch {
            id: "b",
            calls: &calls,
            results: &results,
        };
        let plan = runtime.presentation_plan(&batch, &[], &config, Path::new("."));
        let presentations = runtime.present_batch(&plan, 1000);
        (results, presentations)
    };
    // A batch under the cap is untouched.
    let (results, presentations) = present(4);
    for (result, presentation) in results.iter().zip(&presentations) {
        assert_eq!(presentation.text, result.output);
        assert!(presentation.complete);
    }
    // A batch over it shares the cap: every result is cut, none starved.
    let (results, presentations) = present(8);
    let total: usize = presentations
        .iter()
        .map(|presentation| presentation.text.len())
        .sum();
    let limit = AgentLoopConfig::DEFAULT_MAX_BATCH_RESULT_BYTES;
    assert!(total <= limit + 8 * 128, "{total}");
    assert!(
        total
            < results
                .iter()
                .map(|result| result.output.len())
                .sum::<usize>()
    );
    assert!(presentations
        .iter()
        .all(|presentation| !presentation.complete && presentation.text.len() > limit / 16));
}

/// How a batch of `(tool, output)` results is presented at `scale` per mille
/// over `base` history.
fn present_results(
    base: &[ProviderMessage],
    results: &[(&str, String)],
    scale: usize,
) -> Vec<crate::tools::ToolPresentation> {
    let runtime = Runtime::new();
    let config = AgentLoopConfig::default();
    let calls: Vec<ProviderToolCall> = results
        .iter()
        .enumerate()
        .map(|(index, (name, _))| tool_call(&format!("c{index}"), name, "{}"))
        .collect();
    let results: Vec<ToolResult> = results
        .iter()
        .map(|(name, output)| ToolResult::ok(*name, output.clone()))
        .collect();
    let batch = presentation::PresentationBatch {
        id: "b",
        calls: &calls,
        results: &results,
    };
    let plan = runtime.presentation_plan(&batch, base, &config, Path::new("."));
    runtime.present_batch(&plan, scale)
}

#[test]
fn small_and_repeated_results_are_not_charged_to_the_batch_cap() {
    let page = |letter: &str| letter.repeat(16 * 1024);
    // Six large reads and a receipt: the receipt is under an equal share of
    // the cap and reaches the model whole; the reads share the rest.
    let receipt =
        "written a.txt; bytes=9; sha256=abcdef012345; exists=true; do not re-read".to_owned();
    let mut batch: Vec<(&str, String)> = ["a", "b", "c", "d", "e", "f"]
        .iter()
        .map(|letter| ("read", page(letter)))
        .collect();
    batch.push(("write", receipt.clone()));
    let presented = present_results(&[], &batch, 1000);
    assert_eq!(presented[6].text, receipt);
    assert!(presented[6].complete);
    let reads: usize = presented[..6].iter().map(|p| p.text.len()).sum();
    assert!(reads <= AgentLoopConfig::DEFAULT_MAX_BATCH_RESULT_BYTES + 6 * 128);
    assert!(presented[..6]
        .iter()
        .all(|presentation| !presentation.complete && presentation.text.len() > 8 * 1024));

    // Two of five reads are already in context: they cost a pointer, so the
    // three new ones fit the cap and are not cut.
    let base = [
        ProviderMessage::tool("read", "old-a", page("a")),
        ProviderMessage::tool("read", "old-b", page("b")),
    ];
    let batch: Vec<(&str, String)> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|letter| ("read", page(letter)))
        .collect();
    let presented = present_results(&base, &batch, 1000);
    let pointer = duplicate_pointer("read");
    assert_eq!(presented[0].text, pointer);
    assert_eq!(presented[1].text, pointer);
    for (presentation, letter) in presented[2..].iter().zip(["c", "d", "e"]) {
        assert_eq!(presentation.text, page(letter));
        assert!(presentation.complete);
    }
    // The same output twice in one batch: the second is a pointer too.
    let batch = vec![("read", page("a")); 5];
    let presented = present_results(&[], &batch, 1000);
    assert_eq!(presented[0].text, page("a"));
    assert!(presented[1..].iter().all(|p| p.text == pointer));
}

#[test]
fn a_cut_result_is_never_longer_than_the_result_it_cuts() {
    let batch = vec![
        ("shell", "exit 0\nok".to_owned()),
        ("read", "tiny".to_owned()),
        ("read", "x".repeat(4000)),
    ];
    // Even with no room at all, a small result is not swapped for a notice
    // that is longer than it is.
    for scale in [0, 1, 10, 1000] {
        let presented = present_results(&[], &batch, scale);
        for ((_, output), presentation) in batch.iter().zip(&presented) {
            assert!(
                presentation.complete || presentation.text.len() < output.len(),
                "scale {scale}: {} bytes for {}",
                presentation.text.len(),
                output.len()
            );
        }
        assert_eq!(presented[0].text, "exit 0\nok");
        assert_eq!(presented[1].text, "tiny");
    }
}

#[test]
fn anthropic_refusal_and_context_window_stops_are_filtered_and_truncated() {
    use recovery::ProviderTurnStop;
    for (reason, stop) in [
        ("refusal", ProviderTurnStop::Filtered),
        ("model_context_window_exceeded", ProviderTurnStop::Truncated),
        ("max_tokens", ProviderTurnStop::Truncated),
        ("end_turn", ProviderTurnStop::Normal),
    ] {
        assert_eq!(classify_provider_stop_reason(reason, &[]).unwrap(), stop);
    }
    assert!(classify_provider_stop_reason("pause_turn_unknown", &[]).is_err());
}

#[test]
fn the_models_own_todo_updates_do_not_spend_the_stale_list_reminder() {
    let item = |status: &str| {
        vec![crate::TodoChangedItem {
            reason: None,
            id: Some(1),
            title: "verify".into(),
            status: status.into(),
        }]
    };
    let mut cadence = TodoCadence::default();
    // Creation, then two updates of the model's own: only creation is nudged.
    assert!(cadence.after_batch(&item("pending")));
    assert!(!cadence.after_batch(&item("in_progress")));
    let mut updated = item("in_progress");
    updated[0].reason = Some("waiting".into());
    assert!(!cadence.after_batch(&updated));
    // The list then goes stale: the second reminder is still available.
    for _ in 0..3 {
        assert!(!cadence.after_batch(&updated));
    }
    assert!(cadence.after_batch(&updated));
    assert!(!cadence.after_batch(&updated));
}

#[test]
fn skill_listing_keeps_invalid_skill_diagnostics_accessible() {
    let root = TempRoot::new("skill-diagnostics");
    let skill_root = root.join("skills");
    std::fs::create_dir_all(skill_root.join("broken")).unwrap();
    std::fs::write(skill_root.join("broken/SKILL.md"), "invalid").unwrap();
    let mut runtime = Runtime::new();
    for with_valid in [false, true] {
        if with_valid {
            std::fs::create_dir_all(skill_root.join("valid")).unwrap();
            std::fs::write(
                skill_root.join("valid/SKILL.md"),
                "---\nname: valid\ndescription: works\n---\nbody",
            )
            .unwrap();
        }
        let discovery =
            crate::skills::discover(&[crate::skills::SkillRoot::new(&skill_root, 0)]).unwrap();
        let listing = render_skill_list(&discovery);
        assert!(listing.success);
        assert!(listing.output.contains("broken"));
        assert!(listing.output.contains("missing frontmatter"));
        assert_eq!(listing.output.contains("valid: works"), with_valid);
        runtime.skill_discovery_cache = Some((root.to_path_buf(), Some(discovery)));
        assert!(runtime
            .workspace_tool_definitions(crate::OperatingMode::Auto, &root)
            .iter()
            .any(|tool| tool["name"] == "skill"));
    }
}

#[test]
fn skill_profile_is_discovered_once_and_refreshed_next_run() {
    let root = TempRoot::new("skill-profile");
    let mut runtime = Runtime::new();
    runtime.prepare_loop_capabilities(&root).unwrap();
    runtime.skill_discovery_cache = Some((root.to_path_buf(), Some(DiscoveryResult::default())));
    let has_skill = |runtime: &Runtime| {
        runtime
            .workspace_tool_definitions(crate::OperatingMode::Auto, &root)
            .iter()
            .any(|tool| tool["name"] == "skill")
    };
    assert!(!has_skill(&runtime));
    let skill = root.join(".slim/skills/example");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: example\ndescription: Example\n---\nDo work.\n",
    )
    .unwrap();
    assert!(!has_skill(&runtime), "profile must not change mid-run");
    runtime.prepare_loop_capabilities(&root).unwrap();
    assert!(has_skill(&runtime));
}

#[test]
fn todo_tool_keeps_initial_status_and_updates_the_requested_id() {
    // The advertised contract has one batch form; legacy calls still parse.
    let definition = todo_tool_definition();
    let schema = &definition["input_schema"];
    assert_eq!(schema["required"], json!(["todos"]));
    assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
    assert_eq!(schema["properties"]["todos"]["minItems"], 1);
    let mut runtime = Runtime::new();
    let cwd = std::env::temp_dir();
    runtime.prepare_loop_capabilities(&cwd).unwrap();
    let (created, seq) = runtime
        .execute_todo(
            crate::OperatingMode::Auto,
            ToolInvocation {
                batch_id: "todo-test",
                call_id: "create",
                name: "todo",
                arguments: &json!({"todos":[
                    {"title":"inspect", "status":"in_progress"},
                    {"title":"implement", "status":"pending"},
                    {"title":"verify", "status":"pending"}
                ]})
                .to_string(),
            },
            1,
        )
        .unwrap();
    assert!(created.success, "{}", created.output);
    let items = runtime
        .capability_bridge
        .as_ref()
        .unwrap()
        .todo("session")
        .unwrap()
        .items();
    let first = items[0].id;
    let second = items[1].id;
    assert_eq!(items[0].status, crate::task::TodoStatus::InProgress);
    assert!(created
        .output
        .contains(&format!("todo {first} [in_progress]: inspect")));
    // A follow-up turn must retain IDs, status and task revisions.
    runtime.prepare_loop_capabilities(&cwd).unwrap();
    let persisted = serde_json::to_string(&runtime.task_facts()).unwrap();
    let facts: Vec<DurableFact> = serde_json::from_str(&persisted).unwrap();
    runtime = Runtime::new();
    runtime.restore_task_facts(&facts, &cwd).unwrap();
    assert!(
        runtime.app.events().is_empty(),
        "restoration must not execute tools"
    );
    let (updated, _) = runtime
        .execute_todo(
            crate::OperatingMode::Auto,
            ToolInvocation {
                batch_id: "todo-test",
                call_id: "update",
                name: "todo",
                arguments: &json!({"todos":[
                    {"id":first.to_string(), "title":"inspect", "status":"completed"},
                    {"id":second, "status":"in_progress"}
                ]})
                .to_string(),
            },
            seq,
        )
        .unwrap();
    assert!(updated.success, "{}", updated.output);
    let items = runtime
        .capability_bridge
        .as_ref()
        .unwrap()
        .todo("session")
        .unwrap()
        .items();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].status, crate::task::TodoStatus::Completed);
    assert_eq!(items[1].status, crate::task::TodoStatus::InProgress);
    assert_eq!(items[2].status, crate::task::TodoStatus::Pending);
}

#[test]
fn todo_block_reason_is_validated_published_and_restored() {
    let cwd = std::env::temp_dir();
    let mut runtime = Runtime::new();
    runtime.prepare_loop_capabilities(&cwd).unwrap();
    let call = |runtime: &mut Runtime, id: &str, args: Value, seq| {
        runtime
            .execute_todo(
                crate::OperatingMode::Auto,
                ToolInvocation {
                    batch_id: "reason-test",
                    call_id: id,
                    name: "todo",
                    arguments: &args.to_string(),
                },
                seq,
            )
            .unwrap()
    };
    let (_, seq) = call(&mut runtime, "create", json!({"title":"deploy"}), 1);
    let id = runtime.todo_items()[0].id.unwrap();
    let (blocked, seq) = call(
        &mut runtime,
        "block",
        json!({"id":id,"status":"blocked","reason":"missing approval"}),
        seq,
    );
    assert!(blocked.success);
    assert_eq!(
        runtime.todo_items()[0].reason.as_deref(),
        Some("missing approval")
    );
    assert!(runtime.app.events().iter().any(|event| matches!(&event.kind,
            crate::EventKind::TodoChanged { items } if items[0].reason.as_deref() == Some("missing approval"))));
    let before = runtime.todo_items();
    for reason in [json!(""), json!("x".repeat(1025)), json!(42)] {
        assert!(
            parse_todo_mutations(&json!({"id":id,"status":"blocked","reason":reason})).is_err()
        );
    }
    assert!(parse_todo_mutations(
        &json!({"title":"deploy","status":"blocked","reason":"unknown ID"})
    )
    .is_err());
    let facts = runtime.task_facts();
    runtime = Runtime::new();
    runtime.restore_task_facts(&facts, &cwd).unwrap();
    assert_eq!(runtime.todo_items(), before);
    let (resumed, _) = call(
        &mut runtime,
        "resume",
        json!({"id":id,"status":"in_progress"}),
        seq,
    );
    assert!(resumed.success);
    assert_eq!(runtime.todo_items()[0].reason, None);
}

#[test]
fn todo_tool_reports_invalid_entries_and_publishes_partial_progress() {
    let mut runtime = Runtime::new();
    runtime
        .prepare_loop_capabilities(&std::env::temp_dir())
        .unwrap();
    let call = |runtime: &mut Runtime, id: &str, args: Value, seq| {
        runtime
            .execute_todo(
                crate::OperatingMode::Auto,
                ToolInvocation {
                    batch_id: "todo-test",
                    call_id: id,
                    name: "todo",
                    arguments: &args.to_string(),
                },
                seq,
            )
            .unwrap()
    };
    let (result, mut seq) = call(
        &mut runtime,
        "create",
        json!({"todos":[
            {"title":"first", "status":"in_progress"}, {"title":"second"}
        ]}),
        1,
    );
    assert!(result.success, "{}", result.output);
    let before = runtime
        .capability_bridge
        .as_ref()
        .unwrap()
        .todo("session")
        .unwrap()
        .items()
        .to_vec();
    let first = before[0].id;
    let second = before[1].id;
    let cases = [
        (
            json!({"todos":[{"id":first,"status":"completed"},{"title":"bad","status":"invalid"}]}),
            "entry 2",
        ),
        (json!({"id":99,"status":"completed"}), "todo 99"),
        (
            json!({"id":second,"status":"in_progress"}),
            "only one todo may be in progress",
        ),
    ];
    for (index, (args, expected)) in cases.into_iter().enumerate() {
        let (result, next) = call(&mut runtime, &format!("invalid-{index}"), args, seq);
        seq = next;
        assert!(!result.success);
        assert!(result.output.contains(expected), "{}", result.output);
        assert_eq!(
            runtime
                .capability_bridge
                .as_ref()
                .unwrap()
                .todo("session")
                .unwrap()
                .items(),
            before
        );
    }
    let (result, seq) = call(
        &mut runtime,
        "partial",
        json!({"todos":[
            {"id":first,"status":"completed"}, {"id":99,"status":"in_progress"}
        ]}),
        seq,
    );
    assert!(!result.success);
    assert!(result
        .output
        .contains(&format!("todo {first} [completed]: first")));
    let items = runtime
        .app
        .events()
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            crate::EventKind::TodoChanged { items } => Some(items),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        items[0].status, "completed",
        "UI must reflect a mutation even when a later entry fails"
    );
    let (result, _) = call(
        &mut runtime,
        "recover",
        json!({"id":second,"status":"in_progress"}),
        seq,
    );
    assert!(result.success, "{}", result.output);
}

#[test]
fn todo_parser_accepts_content_numeric_id_and_string_list() {
    let content = parse_todo_mutations(&json!({"content": "map the leak"})).unwrap();
    assert_eq!(content.len(), 1);
    assert!(matches!(
        &content[0].1,
        TaskMutation::TodoAdd { title, .. } if title == "map the leak"
    ));

    let numbered = parse_todo_mutations(&json!({
        "todos": [{"id": 1, "status": "inProgress"}]
    }))
    .unwrap();
    assert_eq!(numbered.len(), 1);
    assert!(matches!(
        numbered[0].1,
        TaskMutation::TodoSetStatus {
            reason: None,
            id: Some(1),
            status: TaskTodoStatus::InProgress
        }
    ));

    let listed = parse_todo_mutations(&json!({"todos": ["ship n2", "verify gate"]})).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].0, "ship n2");
    assert_eq!(listed[1].0, "verify gate");
}

#[test]
fn todo_full_list_resend_updates_matching_titles_in_place() {
    let mut runtime = Runtime::new();
    let cwd = std::env::temp_dir();
    runtime.prepare_loop_capabilities(&cwd).unwrap();
    let mut seq = 1;
    let mut call = |call_id: &str, args: Value| {
        let (result, next) = runtime
            .execute_todo(
                crate::OperatingMode::Auto,
                ToolInvocation {
                    batch_id: "todo-test",
                    call_id,
                    name: "todo",
                    arguments: &args.to_string(),
                },
                seq,
            )
            .unwrap();
        seq = next;
        result
    };
    let created = call(
        "create",
        json!({"todos":[
            {"title":"inspect", "status":"in_progress"},
            {"title":"implement"},
            {"title":"verify"}
        ]}),
    );
    assert!(created.success, "{}", created.output);
    // Full-list resend without ids: an entry whose title matches exactly
    // one existing item is that item's status update, not a duplicate.
    let resent = call(
        "resend",
        json!({"todos":[
            {"title":"inspect", "status":"completed"},
            {"title":"implement", "status":"in_progress"},
            {"title":"verify", "status":"pending"},
            {"title":"deploy", "status":"pending"}
        ]}),
    );
    assert!(resent.success, "{}", resent.output);
    let items = runtime
        .capability_bridge
        .as_ref()
        .unwrap()
        .todo("session")
        .unwrap()
        .items()
        .to_vec();
    assert_eq!(items.len(), 4, "{:?}", items);
    assert_eq!(items[0].status, crate::task::TodoStatus::Completed);
    assert_eq!(items[1].status, crate::task::TodoStatus::InProgress);
    assert_eq!(items[2].status, crate::task::TodoStatus::Pending);
    assert_eq!(items[3].title, "deploy");
}

#[test]
fn single_redact_removes_sensitive_values_before_governor_relay() {
    let mut runtime = Runtime::new();
    runtime.register_sensitive_value("private-answer");
    let once = runtime.redact_sensitive("selected private-answer");
    assert_eq!(once, "selected [REDACTED]");
    assert_eq!(runtime.redact_sensitive(&once), once);
}

#[tokio::test]
async fn batch_prepare_dedups_identical_raw_calls_in_order() {
    let runtime = Runtime::new();
    let calls = vec![
        tool_call("one", "read", r#"{"path":".","max_lines":1}"#),
        tool_call("two", "read", r#"{"path":".","max_lines":1}"#),
        tool_call("three", "list", r#"{"path":".","max_entries":1}"#),
    ];
    let prepared = runtime
        .prepare_provider_tool_invocations(
            crate::OperatingMode::Auto,
            std::env::temp_dir().as_path(),
            &calls,
        )
        .await
        .expect("batch prepare");
    assert_eq!(prepared.len(), 3);
    assert_eq!(
        prepared[0].canonical_fingerprint,
        prepared[1].canonical_fingerprint
    );
    assert_eq!(prepared[0].target_paths, prepared[1].target_paths);
    assert_ne!(
        prepared[0].canonical_fingerprint,
        prepared[2].canonical_fingerprint
    );
}

#[tokio::test]
async fn search_context_identity_results_and_response_budget() {
    let root = TempRoot::new("context-runtime");
    std::fs::write(
        root.join("data.txt"),
        format!(
            "{}\nneedle\n{}\n",
            "prefix".repeat(200),
            "suffix".repeat(200)
        ),
    )
    .unwrap();
    let mut runtime = Runtime::with_artifact_store(root.join(".slim/artifacts")).unwrap();
    let calls = [None, Some(0), Some(1), Some(3)]
        .into_iter()
        .enumerate()
        .map(|(index, context)| {
            let mut arguments = serde_json::json!({"path":"data.txt", "query":"needle"});
            if let Some(context) = context {
                arguments["context_lines"] = serde_json::json!(context);
            }
            provider_call(&format!("search-{index}"), "search", arguments)
        })
        .collect::<Vec<_>>();
    let prepared = runtime
        .prepare_provider_tool_invocations(crate::OperatingMode::Auto, &root, &calls)
        .await
        .unwrap();
    assert_eq!(evidence_reuse_aliases(&prepared), vec![0, 0, 2, 3]);
    assert!(prepared.iter().all(|call| call.error.is_none()));
    let mut outputs = Vec::new();
    let mut seq = 1;
    for (call, prepared) in calls.iter().zip(&prepared) {
        let (outcome, next) = runtime
            .execute_tool_call_async(ToolInvocation::provider("fixture", call), prepared, seq)
            .await
            .unwrap();
        seq = next;
        assert!(outcome.result.success);
        assert!(
            outcome.receipt.bytes_read > 0,
            "fresh search must still observe the file"
        );
        outputs.push(outcome.result);
    }
    assert_eq!(outputs[0].output, outputs[1].output);
    assert!(!outputs[0].output.contains("prefix"));
    assert!(outputs[2].output.contains("prefix"));
    assert!(outputs[2].output.contains("[truncated "));
    assert_ne!(outputs[2].output, outputs[3].output);
    let budget = 256;
    runtime
        .materialize_results(&mut outputs, budget, None, seq)
        .await
        .unwrap();
    let context = &outputs[2];
    let handle = context.artifact.as_ref().unwrap();
    assert_eq!(
        std::fs::read_to_string(&handle.path).unwrap(),
        context.output
    );
    let preview = present_unstructured(
        "search",
        &context.output,
        PresentationBudget { max_bytes: budget },
    )
    .text;
    assert!(preview.contains("truncated"));
    assert!(preview.len() < context.output.len());
    assert!(preview.contains("aggregate presentation budget"));
}

#[tokio::test]
async fn read_only_alias_keeps_the_current_admission_note() {
    let root = TempRoot::new("admission-alias");
    std::fs::write(root.join("data.txt"), "before\nneedle\nafter\n").unwrap();

    for (label, calls, expected_prefix) in [
        (
            "canonical-first",
            vec![
                provider_call(
                    "canonical",
                    "search",
                    serde_json::json!({
                        "path":"data.txt",
                        "query":"needle",
                        "context_lines":3
                    }),
                ),
                provider_call(
                    "saturated",
                    "search",
                    serde_json::json!({
                        "path":"data.txt",
                        "query":"needle",
                        "context_lines":4
                    }),
                ),
            ],
            (false, true),
        ),
        (
            "saturated-first",
            vec![
                provider_call(
                    "saturated",
                    "search",
                    serde_json::json!({
                        "path":"data.txt",
                        "query":"needle",
                        "context_lines":4
                    }),
                ),
                provider_call(
                    "canonical",
                    "search",
                    serde_json::json!({
                        "path":"data.txt",
                        "query":"needle",
                        "context_lines":3
                    }),
                ),
            ],
            (true, false),
        ),
    ] {
        let mut runtime = Runtime::new();
        let prepared = runtime
            .prepare_provider_tool_invocations(crate::OperatingMode::Auto, &root, &calls)
            .await
            .unwrap_or_else(|error| panic!("{label}: {error:?}"));
        assert_eq!(evidence_reuse_aliases(&prepared), vec![0, 0], "{label}");
        let prefixes = prepared
            .iter()
            .map(|call| crate::tools::admission_output_prefix(&call.admission_notes))
            .collect::<Vec<_>>();
        assert_eq!(prefixes[0].is_some(), expected_prefix.0, "{label}");
        assert_eq!(prefixes[1].is_some(), expected_prefix.1, "{label}");

        let (results, _) = runtime
            .execute_provider_tool_batch(
                crate::OperatingMode::Auto,
                &root,
                label,
                &calls,
                1,
                &mut CausalGovernor::default(),
            )
            .await
            .unwrap_or_else(|error| panic!("{label}: {error:?}"));
        assert_eq!(results.len(), 2, "{label}");
        assert_eq!(
            results[0].output.starts_with("[admission: "),
            expected_prefix.0,
            "{label}"
        );
        assert_eq!(
            results[1].output.starts_with("[admission: "),
            expected_prefix.1,
            "{label}"
        );
        let plain = results
            .iter()
            .map(|result| {
                result
                    .output
                    .strip_prefix("[admission: ")
                    .and_then(|rest| rest.split_once("]\n"))
                    .map(|(_, output)| output)
                    .unwrap_or(result.output.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            plain[0], plain[1],
            "{label}: aliases must reuse the same evidence"
        );
    }
}

#[test]
fn skill_discovery_is_memoized_per_cwd_for_the_run() {
    fn write_skill(root: &std::path::Path, name: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("skill dir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: fixture\n---\nbody\n"),
        )
        .expect("skill fixture");
    }

    let root = TempRoot::new("skill-memo");
    let workspace = root.join("workspace");
    write_skill(&workspace.join(".slim").join("skills"), "first");
    let mut runtime = Runtime::new();
    let first = runtime
        .cached_skill_discovery(&workspace)
        .expect("first discovery");
    assert!(first.active("first").is_some());
    write_skill(&workspace.join(".slim").join("skills"), "second");
    let second = runtime
        .cached_skill_discovery(&workspace)
        .expect("memoized discovery");
    assert!(second.active("second").is_none());
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("other cwd");
    let other = runtime
        .cached_skill_discovery(&elsewhere)
        .expect("other cwd discovery");
    assert!(other.active("first").is_none());
}

#[test]
fn skill_dispatch_validates_list_and_script_types_before_dispatch() {
    let root = TempRoot::new("skill-dispatch-types");
    let skill_dir = root.join(".slim").join("skills").join("fixture");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: fixture\ndescription: dispatch fixture\n---\nfallback body\n",
    )
    .expect("skill metadata");
    let discovery = discover_workspace(&root).expect("skill discovery");
    let dispatch = |arguments: Value| {
        run_skill_dispatch(
            crate::OperatingMode::Auto,
            &root,
            &arguments.to_string(),
            Some(discovery.clone()),
        )
    };

    for value in [json!("true"), Value::Null, json!(1), json!([]), json!({})] {
        let result = dispatch(json!({"list": value, "name": "fixture"}));
        assert!(
            !result.success,
            "invalid list value was dispatched: {result:?}"
        );
        assert_eq!(
            result.output,
            "invalid skill arguments: list must be a boolean"
        );
    }

    for value in [json!(true), Value::Null, json!(1), json!([]), json!({})] {
        let result = dispatch(json!({"list": true, "script": value}));
        assert!(
            !result.success,
            "invalid script value was dispatched: {result:?}"
        );
        assert_eq!(
            result.output,
            "invalid skill arguments: script must be a string"
        );
    }

    for value in [json!(true), Value::Null, json!(1), json!([]), json!({})] {
        let result = dispatch(json!({"name": "fixture", "script": value}));
        assert!(
            !result.success,
            "invalid script value was dispatched: {result:?}"
        );
        assert_eq!(
            result.output,
            "invalid skill arguments: script must be a string"
        );
    }

    let listed = dispatch(json!({"list": true}));
    assert!(listed.success, "list:true failed: {listed:?}");
    assert!(listed.output.contains("fixture: dispatch fixture"));
    assert!(!listed.output.contains("fallback body"));

    for arguments in [
        json!({"name": "fixture"}),
        json!({"list": false, "name": "fixture"}),
        json!({"name": "fixture", "script": ""}),
        json!({"name": "fixture", "script": "run.ps1"}),
        json!({"name": "fixture", "script": "./run.ps1"}),
    ] {
        let result = dispatch(arguments);
        assert!(result.success, "valid skill arguments failed: {result:?}");
        assert!(result.output.contains("fallback body"));
    }

    std::fs::write(skill_dir.join("run.ps1"), "Write-Output 'should-not-run'")
        .expect("skill script");
    let instructions = dispatch(json!({"name": "fixture"}));
    assert!(instructions.success);
    assert!(instructions.output.contains("fallback body"));
    assert!(!instructions.output.contains("should-not-run"));
    let denied = dispatch(json!({"name": "fixture", "script": "run.ps1"}));
    assert!(!denied.success);
    assert!(denied.output.contains("skill requires user trust"));
}

#[test]
fn skill_script_dispatch_never_runs_and_names_the_denial() {
    let root = TempRoot::new("skill-dispatch-denied");
    let skill_dir = root.join(".slim").join("skills").join("fixture");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: fixture\ndescription: dispatch fixture\n---\nbody\n",
    )
    .expect("skill metadata");
    let marker = root.join("ran.txt");
    std::fs::write(
        skill_dir.join("run.ps1"),
        format!("Set-Content -Path '{}' -Value ran", marker.display()),
    )
    .expect("skill script");
    let discovery = discover_workspace(&root).expect("skill discovery");
    let arguments = json!({"name": "fixture", "script": "run.ps1"}).to_string();

    for (mode, expected) in [
        (
            crate::OperatingMode::ReadOnly,
            "skill failed: skill unavailable outside Auto mode",
        ),
        (
            crate::OperatingMode::Auto,
            "skill failed: skill requires user trust",
        ),
    ] {
        let result = run_skill_dispatch(mode, &root, &arguments, Some(discovery.clone()));
        assert!(!result.success);
        assert_eq!(result.output, expected);
    }
    assert!(!marker.exists(), "the model cannot grant user trust");
}

fn summary_response(text: &str, stop_reason: Option<&str>) -> CompactionSummary {
    CompactionSummary {
        text: text.into(),
        stop_reason: stop_reason.map(str::to_owned),
        ..CompactionSummary::default()
    }
}

#[test]
fn compaction_summary_accepts_any_text_that_stopped_normally() {
    // Pi checks how the response ended, not its format.
    assert!(summary_response("plain summary", Some("stop"))
        .validate()
        .is_ok());
    assert!(summary_response("plain summary", Some("end_turn"))
        .validate()
        .is_ok());
}

#[test]
fn compaction_summary_rejects_an_incomplete_response() {
    let rejected = |summary: CompactionSummary| match summary.validate() {
        Err(ProviderError::InvalidResponse { message }) => message,
        other => panic!("expected an invalid response, got {other:?}"),
    };
    assert!(rejected(summary_response("text", None)).contains("without a stop reason"));
    assert!(rejected(summary_response("text", Some("length"))).contains("truncated"));
    assert!(rejected(summary_response("text", Some("content_filter"))).contains("filtered"));
    assert!(rejected(summary_response("text", Some("tool_calls"))).contains("tool"));
    assert!(rejected(summary_response(" \n", Some("stop"))).contains("empty"));
    let mut with_tool_call = summary_response("text", Some("stop"));
    with_tool_call.saw_tool_call = true;
    assert!(rejected(with_tool_call).contains("tool"));
}

#[test]
fn compaction_summary_retains_first_response_timings() {
    let mut summary = CompactionSummary::default();
    summary.push(ProviderEvent::Phase {
        phase: ProviderPhase::FirstByte,
        elapsed_ms: 12,
    });
    summary.push(ProviderEvent::Phase {
        phase: ProviderPhase::FirstSemantic,
        elapsed_ms: 20,
    });

    assert_eq!(summary.time_to_first_byte_ms, Some(12));
    assert_eq!(summary.time_to_first_semantic_ms, Some(20));
}

#[test]
fn multimodal_requests_are_not_eligible_for_text_estimator_calibration() {
    let text = ProviderMessage::user("hello");
    let image = ProviderMessage::user("").with_content_blocks(vec![
        crate::provider::ProviderContentBlock::image("image/png", "aGVsbG8="),
    ]);

    assert!(messages_are_text_only(&[text]));
    assert!(!messages_are_text_only(&[image]));
}

#[test]
fn request_component_bytes_follow_serialized_wire_values() {
    let system = json!({"role": "system", "content": "rules 日本語\n"});
    let history = json!({"role": "user", "content": "ação a\"b"});
    let tool_result = json!({"role": "tool", "content": "linha 👩‍💻\n\\"});
    let tools = json!([{"type": "function", "name": "read", "description": "ler ação\t"}]);
    let body = json!({
        "messages": [&system, &history, &tool_result],
        "tools": &tools,
    })
    .to_string();

    assert_eq!(
        crate::provider::provider_request_component_bytes(&body),
        (
            serde_json::to_vec(&system).expect("system").len() as u64,
            serde_json::to_vec(&tools).expect("tools").len() as u64,
            serde_json::to_vec(&history).expect("history").len() as u64,
            serde_json::to_vec(&tool_result).expect("tool result").len() as u64,
        )
    );
}

#[test]
fn runtime_goal_assurance_requires_validation_after_the_latest_mutation() {
    let progress = |seq, kind, revision| {
        crate::SessionEvent::new(
            seq,
            crate::EventKind::CausalProgressObserved {
                batch_id: "batch".into(),
                call_id: format!("call-{seq}").into(),
                kind,
                tool_name: "shell".into(),
                call_fingerprint: "fingerprint".into(),
                evidence_id: "evidence".into(),
                workspace_revision: revision,
            },
        )
    };
    assert!(!runtime_goal_assurance(&[]));
    assert!(runtime_goal_assurance(&[progress(
        1,
        crate::CausalProgressKind::ValidationGreen,
        0,
    )]));
    assert!(!runtime_goal_assurance(&[
        progress(1, crate::CausalProgressKind::ValidationGreen, 0),
        progress(2, crate::CausalProgressKind::WorkspaceChanged, 1),
    ]));
}

#[test]
fn per_turn_overflow_is_not_a_run_total_hit() {
    let config = AgentLoopConfig {
        max_mutating_tool_calls: 1,
        ..AgentLoopConfig::default()
    };
    let mut calls = vec![
        provider_call("w1", "write", serde_json::json!({"path":"a.txt"})),
        provider_call("w2", "write", serde_json::json!({"path":"b.txt"})),
    ];
    let cut = split_calls_for_budget(&mut calls, config, 0).0;
    assert_eq!(cut.suppressed, 1);
    assert!(!cut.hit_run_total);
    assert_eq!(calls.len(), 1);
    assert!(!should_stop_after_tool_budget_cut(cut, 0, 3));
    assert!(should_stop_after_tool_budget_cut(cut, 0, 1));
}

#[test]
fn run_total_overflow_stops_even_when_turns_remain() {
    let config = AgentLoopConfig {
        max_total_tool_calls: 1,
        ..AgentLoopConfig::default()
    };
    let mut calls = vec![
        provider_call("r1", "read", serde_json::json!({"path":"a.txt"})),
        provider_call("r2", "read", serde_json::json!({"path":"b.txt"})),
    ];
    let cut = split_calls_for_budget(&mut calls, config, 1).0;
    assert_eq!(cut.suppressed, 2);
    assert!(cut.hit_run_total);
    assert!(calls.is_empty());
    assert!(should_stop_after_tool_budget_cut(cut, 0, 8));
}

#[test]
fn fused_call_reserves_two_mutating_slots_before_execution() {
    let fused = provider_call(
        "fusion",
        "write",
        serde_json::json!({
            "path": "a.txt",
            "content": "saved",
            "then_run": {"command": "cargo", "args": ["check"]}
        }),
    );
    assert_eq!(tool_call_slots(&fused), 2);
    let escaped = tool_call(
        "escaped",
        "write",
        r#"{"path":"a.txt","content":"saved","\u0074hen_run":{"command":"cargo","args":["check"]}}"#,
    );
    assert_eq!(tool_call_slots(&escaped), 2);
    let mut one_slot = vec![escaped];
    let cut = split_calls_for_budget(
        &mut one_slot,
        AgentLoopConfig {
            max_mutating_tool_calls: 1,
            ..AgentLoopConfig::default()
        },
        0,
    )
    .0;
    assert_eq!(cut.suppressed, 1);
    assert!(one_slot.is_empty(), "no edit may start without both slots");

    let mut two_slots = vec![
        fused,
        provider_call("next", "patch", serde_json::json!({"path":"b.txt"})),
    ];
    let cut = split_calls_for_budget(
        &mut two_slots,
        AgentLoopConfig {
            max_mutating_tool_calls: 2,
            max_total_tool_calls: 2,
            ..AgentLoopConfig::default()
        },
        0,
    )
    .0;
    assert_eq!(two_slots.len(), 1);
    assert_eq!(cut.suppressed, 1);
    assert!(cut.hit_run_total);
    let mut next_turn = vec![provider_call(
        "next",
        "patch",
        serde_json::json!({"path":"b.txt"}),
    )];
    assert!(
        split_calls_for_budget(
            &mut next_turn,
            AgentLoopConfig {
                max_total_tool_calls: 2,
                ..AgentLoopConfig::default()
            },
            2
        )
        .0
        .hit_run_total
    );
    assert!(next_turn.is_empty());
}

fn prepared_calls(root: &std::path::Path, calls: &[(&str, &str)]) -> Vec<PreparedToolInvocation> {
    let tools = ToolRegistry::default();
    let owned = calls
        .iter()
        .map(|(name, arguments)| ((*name).to_owned(), (*arguments).to_owned()))
        .collect::<Vec<_>>();
    tools.prepare_invocations(crate::OperatingMode::Auto, root, &owned)
}

#[test]
fn phase1_excludes_reads_after_a_same_file_write_or_shell() {
    let root = TempRoot::new("phase1");
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    std::fs::write(root.join("b.txt"), "b\n").unwrap();
    let tools = ToolRegistry::default();
    let prepared = prepared_calls(
        &root,
        &[
            ("write", r#"{"path":"a.txt","content":"A\n"}"#),
            ("read", r#"{"path":"a.txt"}"#),
            ("read", r#"{"path":"b.txt"}"#),
            ("search", r#"{"path":".","query":"A"}"#),
        ],
    );
    assert_eq!(phase1_snapshot_indices(&tools, &prepared), vec![2]);

    let prepared = prepared_calls(
        &root,
        &[
            ("read", r#"{"path":"b.txt"}"#),
            ("shell", r#"{"command":"echo x"}"#),
            ("read", r#"{"path":"a.txt"}"#),
        ],
    );
    assert_eq!(phase1_snapshot_indices(&tools, &prepared), vec![0]);

    let prepared = prepared_calls(
        &root,
        &[
            ("read", r#"{"path":"a.txt"}"#),
            ("read", r#"{"path":"b.txt"}"#),
            ("write", r#"{"path":"a.txt","content":"A\n"}"#),
        ],
    );
    assert_eq!(phase1_snapshot_indices(&tools, &prepared), vec![0, 1]);

    let prepared = prepared_calls(
        &root,
        &[
            ("write", r#"{"path":"a.txt","content":"A\n"}"#),
            ("read", r#"{"path":"b.txt"}"#),
        ],
    );
    assert_eq!(phase1_snapshot_indices(&tools, &prepared), vec![1]);

    let prepared = prepared_calls(
        &root,
        &[
            ("shell", r#"{"command":"echo x"}"#),
            ("read", r#"{"path":"a.txt"}"#),
            ("read", r#"{"path":"b.txt"}"#),
        ],
    );
    assert_eq!(
        phase1_snapshot_indices(&tools, &prepared),
        Vec::<usize>::new()
    );
    assert_eq!(
        phase1_snapshot_indices_ready(&tools, &prepared, 1, &vec![None; prepared.len()]),
        vec![1, 2]
    );
}

#[test]
fn phase1_excludes_code_intel_after_any_workspace_mutation() {
    let root = TempRoot::new("phase1-code-intel");
    std::fs::write(root.join("a.rs"), "fn target() {}\n").unwrap();
    std::fs::write(root.join("b.rs"), "fn caller() { target(); }\n").unwrap();
    let tools = ToolRegistry::default();
    // A mutation on b.rs must block a semantic query on a.rs: editing the
    // caller changes the references of the symbol being queried. The
    // plain read of a.rs stays lifted: file bytes are path-scoped.
    let prepared = prepared_calls(
        &root,
        &[
            ("write", r#"{"path":"b.rs","content":"fn caller() {}\n"}"#),
            (
                "code_intel",
                r#"{"action":"references","path":"a.rs","line":1,"column":4}"#,
            ),
            ("read", r#"{"path":"a.rs"}"#),
        ],
    );
    assert_eq!(phase1_snapshot_indices(&tools, &prepared), vec![2]);

    // With no prior mutation, independent code_intel calls stay parallel.
    let prepared = prepared_calls(
        &root,
        &[
            (
                "code_intel",
                r#"{"action":"definition","path":"a.rs","line":1,"column":4}"#,
            ),
            (
                "code_intel",
                r#"{"action":"references","path":"b.rs","line":1,"column":14}"#,
            ),
        ],
    );
    assert_eq!(phase1_snapshot_indices(&tools, &prepared), vec![0, 1]);
}

fn write_call(path: &str) -> (&'static str, String) {
    (
        "write",
        serde_json::json!({"path": path, "content": "x\n"}).to_string(),
    )
}

fn cluster_of(calls: &[(&str, String)], done: &[usize], start: usize) -> Vec<usize> {
    let root = TempRoot::new("cluster");
    let borrowed = calls
        .iter()
        .map(|(name, arguments)| (*name, arguments.as_str()))
        .collect::<Vec<_>>();
    let prepared = prepared_calls(&root, &borrowed);
    let mut results: Vec<Option<ToolResult>> = vec![None; calls.len()];
    for &index in done {
        results[index] = Some(ToolResult::ok("write", "done"));
    }
    independent_mutation_cluster(&prepared, &results, start)
}

#[test]
fn mutation_cluster_stops_at_first_non_mutation_or_barrier() {
    let read = ("read", r#"{"path":"a.txt"}"#.to_owned());
    let shell = ("shell", r#"{"command":"echo x"}"#.to_owned());
    assert_eq!(
        cluster_of(
            &[
                write_call("a.txt"),
                write_call("b.txt"),
                read.clone(),
                write_call("c.txt")
            ],
            &[],
            0
        ),
        vec![0, 1]
    );
    assert_eq!(
        cluster_of(
            &[
                write_call("a.txt"),
                write_call("b.txt"),
                shell.clone(),
                write_call("c.txt")
            ],
            &[],
            0
        ),
        vec![0, 1]
    );
    // A start that is not a plain file mutation opens no cluster at all.
    let calls = [read, shell, write_call("a.txt")];
    assert!(cluster_of(&calls, &[], 0).is_empty());
    assert!(cluster_of(&calls, &[], 1).is_empty());
    assert_eq!(cluster_of(&calls, &[], 2), vec![2]);
}

#[test]
fn mutation_cluster_never_repeats_a_target_file() {
    let calls = [
        write_call("a.txt"),
        write_call("b.txt"),
        write_call("a.txt"),
        write_call("c.txt"),
    ];
    assert_eq!(cluster_of(&calls, &[], 0), vec![0, 1]);
    assert_eq!(cluster_of(&calls, &[], 2), vec![2, 3]);
}

#[test]
fn mutation_cluster_keeps_call_order_and_skips_completed_calls() {
    let calls = [
        write_call("a.txt"),
        write_call("b.txt"),
        write_call("c.txt"),
        write_call("d.txt"),
    ];
    assert_eq!(cluster_of(&calls, &[1], 0), vec![0, 2, 3]);
    assert_eq!(cluster_of(&calls, &[0, 1], 2), vec![2, 3]);
}

#[test]
fn mutation_cluster_never_repeats_an_identical_nested_path() {
    let calls = [
        write_call("sub/a.txt"),
        write_call("sub/b.txt"),
        write_call("sub/a.txt"),
    ];
    assert_eq!(cluster_of(&calls, &[], 0), vec![0, 1]);
    assert_eq!(cluster_of(&calls, &[], 2), vec![2]);
}

#[cfg(windows)]
#[test]
fn mutation_cluster_treats_case_variants_of_a_new_file_as_one_file_on_windows() {
    let calls = [
        write_call("New.txt"),
        write_call("new.txt"),
        write_call("other.txt"),
    ];
    assert_eq!(cluster_of(&calls, &[], 0), vec![0]);
    assert_eq!(cluster_of(&calls, &[], 1), vec![1, 2]);
}

#[cfg(windows)]
#[test]
fn phase1_blocks_a_read_after_a_case_variant_write_on_windows() {
    let root = TempRoot::new("phase1-case");
    let tools = ToolRegistry::default();
    let prepared = prepared_calls(
        &root,
        &[
            ("write", r#"{"path":"New.txt","content":"x\n"}"#),
            ("read", r#"{"path":"new.txt"}"#),
        ],
    );
    assert_eq!(
        phase1_snapshot_indices(&tools, &prepared),
        Vec::<usize>::new()
    );
}

fn provider_call(id: &str, name: &str, arguments: serde_json::Value) -> ProviderToolCall {
    tool_call(id, name, arguments.to_string())
}

#[tokio::test]
async fn independent_writes_apply_and_same_file_stays_ordered() {
    let root = TempRoot::new("mut-batch");
    std::fs::write(root.join("a.txt"), "old-a\n").unwrap();
    std::fs::write(root.join("b.txt"), "old-b\n").unwrap();
    let mut runtime = Runtime::new();
    let (results, next_seq) = runtime
        .execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "batch",
            &[
                provider_call(
                    "wa",
                    "write",
                    json!({"path":"a.txt","content":"new-a\n","expected":"old-a\n"}),
                ),
                provider_call(
                    "wb",
                    "write",
                    json!({"path":"b.txt","content":"new-b\n","expected":"old-b\n"}),
                ),
            ],
            1,
            &mut CausalGovernor::default(),
        )
        .await
        .unwrap();
    assert!(results[0].success, "{}", results[0].output);
    assert!(results[1].success, "{}", results[1].output);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "new-a\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).unwrap(),
        "new-b\n"
    );

    let (results, _) = runtime
        .execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "batch2",
            &[
                provider_call(
                    "w1",
                    "write",
                    json!({"path":"a.txt","content":"mid-a\n","expected":"new-a\n"}),
                ),
                provider_call(
                    "w2",
                    "write",
                    json!({"path":"a.txt","content":"final-a\n","expected":"mid-a\n"}),
                ),
            ],
            next_seq,
            &mut CausalGovernor::default(),
        )
        .await
        .unwrap();
    assert!(results[0].success, "{}", results[0].output);
    assert!(results[1].success, "{}", results[1].output);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "final-a\n"
    );
}

#[tokio::test]
async fn same_file_read_after_write_sees_new_bytes_when_other_reads_are_lifted() {
    let root = TempRoot::new("phase1-order");
    std::fs::write(root.join("a.txt"), "old-a\n").unwrap();
    std::fs::write(root.join("b.txt"), "keep-b\n").unwrap();
    let mut runtime = Runtime::new();
    let (results, _) = runtime
        .execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "batch",
            &[
                provider_call(
                    "w",
                    "write",
                    json!({"path":"a.txt","content":"new-a\n","expected":"old-a\n"}),
                ),
                provider_call("ra", "read", json!({"path":"a.txt"})),
                provider_call("rb", "read", json!({"path":"b.txt"})),
            ],
            1,
            &mut CausalGovernor::default(),
        )
        .await
        .unwrap();
    assert!(results[0].success, "{}", results[0].output);
    assert!(results[1].success, "{}", results[1].output);
    assert!(results[2].success, "{}", results[2].output);
    assert_eq!(results[1].output, "new-a\n");
    assert_eq!(results[2].output, "keep-b\n");
}

fn stream_secret_deltas(events: &[ProviderEvent]) -> (Vec<String>, Vec<String>) {
    let mut app = AppHandle::fake();
    let mut normalizer =
        ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec!["sk-secret".into()]);
    for event in events {
        normalizer.push(&mut app, event.clone());
    }
    normalizer.push(
        &mut app,
        ProviderEvent::Stopped {
            reason: "stop".into(),
        },
    );
    normalizer.finish(&mut app).unwrap();
    let mut text = Vec::new();
    let mut reasoning = Vec::new();
    for event in app.events() {
        match &event.kind {
            crate::EventKind::AssistantTextDelta { text: delta } => text.push(delta.clone()),
            crate::EventKind::ReasoningDelta { text: delta } => reasoning.push(delta.clone()),
            _ => {}
        }
    }
    (text, reasoning)
}

#[test]
fn a_secret_split_across_a_reasoning_switch_is_never_emitted_in_pieces() {
    let (text, _) = stream_secret_deltas(&[
        ProviderEvent::TextDelta("token sk-sec".into()),
        ProviderEvent::ReasoningDelta("hmm".into()),
        ProviderEvent::TextDelta("ret done".into()),
    ]);
    assert_eq!(text.concat(), "token [REDACTED] done", "{text:?}");
    assert!(
        text.iter().all(|delta| !delta.contains("sk-")),
        "no delta may carry a secret prefix: {text:?}"
    );
}

#[test]
fn a_reasoning_tail_that_could_start_a_secret_is_released_inside_its_lifecycle() {
    // "s" could begin "sk-secret", so the tail is held while the block streams, but
    // it must still be published before ThinkingEnded: a delta after the end event
    // would fall outside the lifecycle the UI relies on.
    let mut app = AppHandle::fake();
    let mut normalizer =
        ProviderStreamNormalizer::new(ProviderKind::OpenAiCompatible, 1, vec!["sk-secret".into()]);
    for event in [
        ProviderEvent::ReasoningDelta("plan s".into()),
        ProviderEvent::TextDelta("visible".into()),
    ] {
        normalizer.push(&mut app, event);
    }
    normalizer.push(
        &mut app,
        ProviderEvent::Stopped {
            reason: "stop".into(),
        },
    );
    normalizer.finish(&mut app).unwrap();
    let mut open = false;
    let mut released = String::new();
    for event in app.events() {
        match &event.kind {
            crate::EventKind::ThinkingStarted => open = true,
            crate::EventKind::ThinkingEnded => open = false,
            crate::EventKind::ReasoningDelta { text } => {
                assert!(open, "reasoning delta outside its lifecycle: {text:?}");
                released.push_str(text);
            }
            _ => {}
        }
    }
    assert_eq!(released, "plan s");
}

#[test]
fn a_held_secret_prefix_is_released_when_the_stream_stops() {
    let (text, _) = stream_secret_deltas(&[
        ProviderEvent::TextDelta("ends with sk-sec".into()),
        ProviderEvent::ReasoningDelta("hmm".into()),
    ]);
    assert_eq!(text.concat(), "ends with sk-sec", "{text:?}");
}

#[test]
fn largest_fitting_scale_finds_the_boundary_and_keeps_the_floor() {
    use super::presentation::largest_fitting_scale;
    let fits_up_to = |limit: usize| move |scale: &usize| *scale <= limit;
    assert_eq!(largest_fitting_scale(|scale| scale, fits_up_to(1000)), 1000);
    assert_eq!(largest_fitting_scale(|scale| scale, fits_up_to(999)), 999);
    assert_eq!(largest_fitting_scale(|scale| scale, fits_up_to(640)), 640);
    assert_eq!(largest_fitting_scale(|scale| scale, fits_up_to(1)), 1);
    assert_eq!(largest_fitting_scale(|scale| scale, fits_up_to(0)), 0);
    // Nothing fits, not even scale 0: the floor is still returned.
    assert_eq!(largest_fitting_scale(|scale| scale, |_| false), 0);
}

#[test]
fn recovery_budget_gates_retries_on_both_limits() {
    let mut recovery = RecoveryBudget::new(0);
    assert!(recovery.can_retry(0));
    assert!(recovery.can_retry(MAX_PROVIDER_RECOVERIES - 1));
    assert!(!recovery.can_retry(MAX_PROVIDER_RECOVERIES));
    recovery.automatic_recoveries = MAX_AUTOMATIC_RECOVERIES - 1;
    assert!(recovery.can_retry(0));
    recovery.automatic_recoveries = MAX_AUTOMATIC_RECOVERIES;
    assert!(!recovery.can_retry(0));
}

#[test]
fn terminal_recovery_reason_keeps_its_precedence() {
    use super::recovery::terminal_recovery_reason;
    let empty = ProviderError::InvalidResponse {
        message: EMPTY_RESPONSE_MESSAGE.into(),
    };
    let transport = ProviderError::Transport {
        message: "reset".into(),
        safe_to_retry: true,
    };
    let mut recovery = RecoveryBudget::new(0);
    recovery.empty_recovery_used = true;
    recovery.automatic_recoveries = MAX_AUTOMATIC_RECOVERIES;
    recovery.provider_recoveries = MAX_PROVIDER_RECOVERIES;
    assert_eq!(
        terminal_recovery_reason(&empty, &recovery, true),
        "repeated empty provider response"
    );
    assert_eq!(
        terminal_recovery_reason(&transport, &recovery, true),
        "tool effects were emitted; request was not replayed"
    );
    assert_eq!(
        terminal_recovery_reason(&transport, &recovery, false),
        "global automatic recovery limit reached"
    );
    recovery.automatic_recoveries = 0;
    assert_eq!(
        terminal_recovery_reason(&transport, &recovery, false),
        "consecutive provider recovery limit reached"
    );
    recovery.provider_recoveries = 0;
    assert_eq!(
        terminal_recovery_reason(&transport, &recovery, false),
        "automatic recovery stopped"
    );
}

mod stream_hot_path_equivalence {
    use super::*;

    fn reference_split(input: &str, sensitive_values: &[String]) -> usize {
        let held_bytes = sensitive_values
            .iter()
            .flat_map(|value| {
                value
                    .char_indices()
                    .skip(1)
                    .map(move |(index, _)| &value[..index])
            })
            .filter(|prefix| input.ends_with(prefix))
            .map(str::len)
            .max()
            .unwrap_or(0);
        let mut split_at = input.len().saturating_sub(held_bytes);
        loop {
            let adjusted = sensitive_values
                .iter()
                .flat_map(|value| {
                    input
                        .match_indices(value)
                        .map(move |(start, _)| (start, start + value.len()))
                })
                .filter(|(start, end)| *start < split_at && split_at < *end)
                .map(|(start, _)| start)
                .min()
                .unwrap_or(split_at);
            if adjusted == split_at {
                return split_at;
            }
            split_at = adjusted;
        }
    }

    fn reference_chunk(
        pending: &mut String,
        delta: &str,
        sensitive_values: &[String],
        flush: bool,
    ) -> String {
        pending.push_str(delta);
        if sensitive_values.is_empty() {
            return std::mem::take(pending);
        }
        let split_at = if flush {
            pending.len()
        } else {
            reference_split(pending, sensitive_values)
        };
        let tail = pending[split_at..].to_owned();
        let ready = redact_values(sensitive_values, &pending[..split_at]);
        *pending = tail;
        ready
    }

    fn secrets() -> Vec<Vec<String>> {
        let sorted = |mut values: Vec<&str>| {
            values.sort_by_key(|value| std::cmp::Reverse(value.len()));
            values.into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        vec![
            Vec::new(),
            sorted(vec!["sk-live-abc", "abc", "segredo", "日本"]),
            sorted(vec!["aaaa", "aa", "ab"]),
        ]
    }

    #[test]
    fn split_matches_the_reference_for_every_substring() {
        let text = "x sk-live-abc y abc sk-live-ab segredo ação aaaab sk-live-abcabc 日本日 segred";
        for values in secrets() {
            for start in (0..=text.len()).filter(|i| text.is_char_boundary(*i)) {
                for end in (start..=text.len()).filter(|i| text.is_char_boundary(*i)) {
                    let input = &text[start..end];
                    assert_eq!(
                        stream_normalizer::safe_stream_split(input, &values),
                        reference_split(input, &values),
                        "{input:?} {values:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn chunks_match_the_reference_step_by_step_with_and_without_a_held_tail() {
        let text = "x sk-live-abc y abc sk-live-ab segredo ação aaaab 日本日 segred";
        let bounds = (0..=text.len())
            .filter(|i| text.is_char_boundary(*i))
            .collect::<Vec<_>>();
        for values in secrets() {
            for &first in &bounds {
                for &second in bounds.iter().filter(|second| **second >= first) {
                    let parts = [&text[..first], &text[first..second], &text[second..]];
                    let (mut old, mut new) = (String::new(), String::new());
                    for (step, part) in parts.into_iter().enumerate() {
                        // A flush between the pieces too, as a stop does for good.
                        for flush in [false, step == 1] {
                            let expected = reference_chunk(&mut old, part, &values, flush);
                            let actual = stream_normalizer::take_redacted_stream_chunk(
                                &mut new, part, &values, flush,
                            );
                            assert_eq!(actual, expected, "{first}/{second} step {step} {values:?}");
                            assert_eq!(new, old, "{first}/{second} step {step} {values:?}");
                        }
                    }
                    assert_eq!(
                        stream_normalizer::take_redacted_stream_chunk(&mut new, "", &values, true),
                        reference_chunk(&mut old, "", &values, true)
                    );
                    assert!(new.is_empty() && old.is_empty());
                }
            }
        }
    }

    fn reference_sensitive(arguments: &str, secrets: &[String]) -> bool {
        const MAX_UNWRAP_DEPTH: u32 = 8;
        fn contains(value: &Value, secret: &str, depth: u32) -> bool {
            match value {
                Value::String(text) => {
                    text.contains(secret)
                        || (depth > 0
                            && serde_json::from_str::<Value>(text)
                                .is_ok_and(|inner| contains(&inner, secret, depth - 1)))
                }
                Value::Array(values) => values.iter().any(|value| contains(value, secret, depth)),
                Value::Object(values) => values
                    .iter()
                    .any(|(key, value)| key.contains(secret) || contains(value, secret, depth)),
                _ => value.to_string().contains(secret),
            }
        }
        let mut parsed = None;
        secrets.iter().any(|secret| {
            !secret.is_empty()
                && (arguments.contains(secret)
                    || parsed
                        .get_or_insert_with(|| serde_json::from_str::<Value>(arguments).ok())
                        .as_ref()
                        .is_some_and(|value| contains(value, secret, MAX_UNWRAP_DEPTH)))
        })
    }

    #[test]
    fn sensitive_tool_arguments_matches_the_per_secret_walk() {
        let mut nested = json!({"path": "secret-token"}).to_string();
        for _ in 0..10 {
            nested = json!({ "inner": nested }).to_string();
        }
        let mut shallow = json!({"path": "secret-token"}).to_string();
        for _ in 0..5 {
            shallow = json!({ "inner": shallow }).to_string();
        }
        let cases = [
            r#"{"path":"a.txt"}"#.to_owned(),
            r#"{"path":"secret-value"}"#.to_owned(),
            r#"{"secret-key":1}"#.to_owned(),
            r#"{"a":["x",{"b":"12345"}],"c":true,"d":null,"e":1.5}"#.to_owned(),
            r#"{"a":1.5,"b":-7,"c":null,"d":false}"#.to_owned(),
            json!({"x": json!({"y": json!({"z": "secret-token"}).to_string()}).to_string()})
                .to_string(),
            json!(json!({"path": "secret-token"}).to_string()).to_string(),
            nested,
            shallow,
            "not json secret-value".to_owned(),
            "not json at all".to_owned(),
            r#"{"path":"a.txt""#.to_owned(),
            String::new(),
        ];
        let lists: [Vec<String>; 6] = [
            vec![],
            vec![String::new()],
            vec!["secret-value".into()],
            vec!["secret-key".into(), "secret-token".into()],
            vec![
                "1.5".into(),
                "-7".into(),
                "null".into(),
                "false".into(),
                "true".into(),
            ],
            vec!["nomatch".into(), String::new(), "12345".into()],
        ];
        for arguments in &cases {
            for secrets in &lists {
                assert_eq!(
                    sensitive_tool_arguments(arguments, secrets),
                    reference_sensitive(arguments, secrets),
                    "{arguments} {secrets:?}"
                );
            }
        }
    }
}
