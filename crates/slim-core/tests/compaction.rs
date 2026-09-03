use std::fs;
use std::path::PathBuf;

use slim_core::context::{
    build_bounded_summary_prompt_with_checkpoint, build_summary_prompt,
    build_summary_prompt_with_checkpoint, compact, compact_provider_messages,
    compaction_prefix_fingerprint, local_emergency_summary, select_compaction_history,
    AdaptiveTokenEstimator, ArtifactStore, CompactionHandle, CompactionPolicy, CompactionStatus,
    ContextItem, PreparedCompaction,
};
use slim_core::provider::{ProviderContentBlock, ProviderMessage, ProviderToolCall};

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

#[test]
fn compaction_policy_defaults_match_small_and_large_context_windows() {
    let policy = CompactionPolicy::default();
    assert!(policy.enabled);
    assert!(policy.background);
    assert_eq!(policy.keep_recent_tokens, 20_000);
    assert_eq!(policy.keep_recent_for_window(32_000), 8_000);
    assert_eq!(policy.keep_recent_for_window(1_000_000), 20_000);
    assert_eq!(policy.soft_threshold_tokens(200_000), 120_000);
    assert_eq!(policy.hard_threshold_tokens(200_000), 170_000);
    assert_eq!(policy.soft_threshold_tokens(1_000_000), 300_000);
    assert_eq!(policy.hard_threshold_tokens(1_000_000), 500_000);
    assert_eq!(policy.summary_max_bytes, 64 * 1024);
    assert_eq!(policy.manual_instructions_max_bytes, 4 * 1024);
}

#[test]
fn selection_keeps_root_and_recent_history_without_starting_on_tool() {
    let large = "x".repeat(80_000);
    let messages = vec![
        ProviderMessage::user("literal root"),
        ProviderMessage::assistant("old", Vec::new()),
        ProviderMessage::user(large),
        ProviderMessage::assistant(
            "recent",
            vec![ProviderToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }],
        ),
        ProviderMessage::tool("read", "call-1", "ok"),
    ];
    let selection = select_compaction_history(&messages, &CompactionPolicy::default())
        .expect("compactable selection");

    assert_eq!(selection.root_instruction, "literal root");
    assert!(selection.first_kept_index > 0);
    assert_ne!(messages[selection.first_kept_index].role, "tool");
    assert_eq!(
        selection
            .kept
            .last()
            .and_then(|m| m.tool_call_id.as_deref()),
        Some("call-1")
    );
}

#[test]
fn summary_prompt_is_structured_chains_checkpoint_and_bounds_tool_results() {
    let output = format!("HEAD{}TAIL", "z".repeat(5_000));
    let messages = vec![
        ProviderMessage::user("goal"),
        ProviderMessage::tool("read", "call-1", output),
    ];
    let prompt = build_summary_prompt_with_checkpoint(&messages, Some("prior checkpoint"));

    for heading in [
        "## Goal",
        "## Constraints",
        "## Progress",
        "## Blocked",
        "## Decisions",
        "## Next steps",
        "## Critical context",
    ] {
        assert!(prompt.contains(heading), "missing {heading}");
    }
    assert!(prompt.contains("[Untrusted previous checkpoint]"));
    assert!(prompt.contains("prior checkpoint"));
    let bounded = build_bounded_summary_prompt_with_checkpoint(
        &messages,
        Some("prior checkpoint"),
        32_000,
        4_096,
    )
    .expect("bounded prompt");
    assert!(bounded.contains("[Untrusted previous checkpoint]"));
    assert!(prompt.contains("HEAD"));
    assert!(prompt.contains("TAIL"));
    assert!(prompt.contains("tool result truncated"));
    assert!(
        prompt.len() < 5_000,
        "tool result should be bounded before request"
    );
}

#[test]
fn selection_formats_text_blocks_without_pinning_non_text_to_kept() {
    let messages = vec![
        ProviderMessage::user("root"),
        ProviderMessage::assistant("old", Vec::new())
            .with_content_blocks(vec![ProviderContentBlock::text("text-block fact")]),
        ProviderMessage::assistant("image", Vec::new()).with_content_blocks(vec![
            ProviderContentBlock::Image {
                media_type: "image/png".into(),
                data: "aW1hZ2U=".into(),
            },
        ]),
        ProviderMessage::user("recent"),
    ];
    let policy = CompactionPolicy {
        keep_recent_tokens: 1,
        ..CompactionPolicy::default()
    };

    let selection = select_compaction_history(&messages, &policy).expect("selection");
    let prompt = build_summary_prompt(&selection.summarized);

    assert_eq!(selection.first_kept_index, 3);
    assert_eq!(selection.kept, messages[3..]);
    assert!(prompt.contains("text-block fact"));
    assert!(local_emergency_summary(&selection).contains("text-block fact"));
}

#[test]
fn compaction_fingerprint_covers_name_and_content_blocks() {
    let original = ProviderMessage::user("content")
        .with_content_blocks(vec![ProviderContentBlock::text("alpha")]);
    let mut renamed = original.clone();
    renamed.name = Some("renamed".into());
    let changed_block = ProviderMessage::user("content")
        .with_content_blocks(vec![ProviderContentBlock::text("beta")]);

    assert_ne!(
        compaction_prefix_fingerprint(std::slice::from_ref(&original)),
        compaction_prefix_fingerprint(&[renamed])
    );
    assert_ne!(
        compaction_prefix_fingerprint(std::slice::from_ref(&original)),
        compaction_prefix_fingerprint(&[changed_block])
    );
}

#[test]
fn prepared_checkpoint_accepts_append_only_history_and_rejects_changed_prefix() {
    let messages = vec![
        ProviderMessage::user("root"),
        ProviderMessage::assistant("old", Vec::new()),
        ProviderMessage::user("recent"),
    ];
    let handle = CompactionHandle::default();
    handle.store_prepared(PreparedCompaction {
        summary: "summary".into(),
        prefix_fingerprint: compaction_prefix_fingerprint(&messages[..2]),
        first_kept_index: 2,
        source_len: messages.len(),
        provider_identity: "openai:model-a".into(),
        input_tokens: 10,
        output_tokens: 2,
        duration_ms: 1,
    });
    let mut appended = messages.clone();
    appended.push(ProviderMessage::assistant("new", Vec::new()));
    assert!(handle.take_prepared(&appended, "openai:model-a").is_some());

    handle.store_prepared(PreparedCompaction {
        summary: "summary".into(),
        prefix_fingerprint: compaction_prefix_fingerprint(&messages[..2]),
        first_kept_index: 2,
        source_len: messages.len(),
        provider_identity: "openai:model-a".into(),
        input_tokens: 10,
        output_tokens: 2,
        duration_ms: 1,
    });
    let mut changed = messages;
    changed[1].content = "changed".into();
    assert!(handle.take_prepared(&changed, "openai:model-a").is_none());
    assert_eq!(handle.status(), CompactionStatus::Discarded);
}

#[test]
fn manual_request_invalidates_prepared_summary_and_is_bounded() {
    let handle = CompactionHandle::default();
    handle.store_prepared(PreparedCompaction {
        summary: "summary".into(),
        prefix_fingerprint: "fingerprint".into(),
        first_kept_index: 1,
        source_len: 2,
        provider_identity: "provider:model".into(),
        input_tokens: 10,
        output_tokens: 2,
        duration_ms: 1,
    });
    handle.request_manual("focus").expect("manual request");
    assert_eq!(handle.manual_instructions().as_deref(), Some("focus"));
    assert!(handle.request_manual("x".repeat(4 * 1024 + 1)).is_err());
    assert!(handle
        .take_prepared(
            &[
                ProviderMessage::user("root"),
                ProviderMessage::user("recent")
            ],
            "provider:model"
        )
        .is_none());
}

#[test]
fn local_emergency_summary_is_bounded_and_non_empty() {
    let messages = vec![
        ProviderMessage::user("literal root"),
        ProviderMessage::assistant("old context", Vec::new()),
        ProviderMessage::user("recent"),
    ];
    let selection =
        select_compaction_history(&messages, &CompactionPolicy::default()).expect("compactable");
    let summary = slim_core::context::local_emergency_summary(&selection);
    assert!(summary.contains("local extract"));
    assert!(!summary.trim().is_empty());
    assert!(summary.len() <= 8 * 1024);
    let compacted = slim_core::context::apply_compaction_selection(&messages, &selection, summary)
        .expect("apply");
    assert!(compacted
        .iter()
        .any(|message| message.content.contains("[Compacted context]")));
}

#[test]
fn adaptive_estimator_does_not_add_overhead_already_present_on_the_wire() {
    let mut estimator = AdaptiveTokenEstimator::default();
    assert_eq!(estimator.estimate("provider", "model", 350), 100);
    for _ in 0..4 {
        estimator.observe("provider", "model", 350, 100);
    }
    assert_eq!(estimator.estimate("provider", "model", 350), 100);
}
