use std::fs;
use std::path::PathBuf;

use slim_core::context::{
    build_summary_prompt, compact, compact_provider_messages, ArtifactStore, ContextItem,
};
use slim_core::provider::{ProviderMessage, ProviderToolCall};

fn temp_dir() -> PathBuf {
    let unique = format!(
        "slim-context-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    std::env::temp_dir().join(unique)
}

#[test]
fn compaction_preserves_task_and_tool_anchors() {
    let items = vec![
        ContextItem::Text("old transcript".into()),
        ContextItem::Todo("todo-1".into()),
        ContextItem::Plan("plan-1".into()),
        ContextItem::Goal("goal-1".into()),
        ContextItem::ToolPair("tool-call-1".into()),
    ];
    let result = compact(&items, "summary from the same model");

    assert_eq!(result.original_count, 5);
    assert_eq!(result.summary, "summary from the same model");
    assert_eq!(result.preserved, items[1..]);
}

#[test]
fn large_output_is_recoverable_by_artifact_handle() {
    let root = temp_dir();
    let store = ArtifactStore::new(&root).expect("store");
    let handle = store.put("tool-output", b"full output").expect("put");
    assert_eq!(store.read(&handle).expect("read"), b"full output");
    assert!(handle.path.exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn provider_compaction_keeps_complete_recent_tool_group_and_drops_orphans() {
    let messages = vec![
        ProviderMessage::user("old transcript"),
        ProviderMessage::assistant(
            "working",
            vec![
                ProviderToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
                ProviderToolCall {
                    id: "call-2".into(),
                    name: "list".into(),
                    arguments: "{}".into(),
                },
            ],
        ),
        ProviderMessage::tool("read", "call-1", "one"),
        ProviderMessage::tool("list", "call-2", "two"),
        ProviderMessage::tool("orphan", "missing", "must not survive"),
    ];
    let compacted = compact_provider_messages(&messages, "stable summary").expect("compact");

    assert_eq!(compacted[0].role, "user");
    assert!(compacted[0].content.contains("old transcript"));
    assert!(compacted[0].content.contains("stable summary"));
    assert_eq!(compacted[1].role, "assistant");
    assert_eq!(compacted[2].tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(compacted[3].tool_call_id.as_deref(), Some("call-2"));
    assert_eq!(compacted.len(), 4);
    assert!(build_summary_prompt(&messages).contains("old transcript"));
}

#[test]
fn compaction_keeps_root_instruction_verbatim_as_an_immutable_anchor() {
    let root = "Use the exact release path: C:\\work\\release\\artifact.zip";
    let messages = vec![
        ProviderMessage::user(root),
        ProviderMessage::assistant(
            "working",
            vec![ProviderToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }],
        ),
        ProviderMessage::tool("read", "call-1", "ok"),
    ];
    let compacted = compact_provider_messages(&messages, "new summary").expect("compact");
    assert!(compacted[0]
        .content
        .starts_with(&format!("[Root instruction]\n{root}")));
    assert!(compacted[0]
        .content
        .contains("[Compacted context]\nnew summary"));
}

#[test]
fn provider_compaction_rejects_degenerate_summary_without_mutating_input() {
    let messages = vec![ProviderMessage::user("keep me")];
    assert!(compact_provider_messages(&messages, " \n ").is_err());
    assert_eq!(messages[0].content, "keep me");
}

#[test]
fn provider_compaction_drops_unmatched_assistant_tool_calls() {
    let messages = vec![
        ProviderMessage::user("old"),
        ProviderMessage::assistant(
            "recent",
            vec![ProviderToolCall {
                id: "missing-result".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }],
        ),
    ];
    let compacted = compact_provider_messages(&messages, "summary").expect("compact");
    assert!(compacted[1].tool_calls.is_empty());
    assert!(compacted.iter().all(|message| message.role != "tool"));
}
