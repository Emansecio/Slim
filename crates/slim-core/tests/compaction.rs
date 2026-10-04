use std::fs;
use std::path::PathBuf;

use slim_core::context::{
    apply_compaction, build_history_summary_request, compaction_prefix_fingerprint,
    compaction_summary_message, compaction_summary_text, estimate_tokens,
    fit_summary_for_persistence, merge_split_turn_summary, prepare_compaction, should_compact,
    AdaptiveTokenEstimator, ArtifactStore, CompactionCommit, CompactionHandle, CompactionPolicy,
    CompactionPreparation, CompactionReason, CompactionSettings, CompactionStatus, ContextUsage,
    FileOperations, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, NO_PRIOR_HISTORY,
    UPDATE_SUMMARIZATION_PROMPT,
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

fn user(text: impl Into<String>) -> ProviderMessage {
    ProviderMessage::user(text)
}

fn assistant(text: impl Into<String>) -> ProviderMessage {
    ProviderMessage::assistant(text, Vec::new())
}

fn call(id: &str, name: &str, arguments: &str) -> ProviderToolCall {
    ProviderToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

fn settings(keep_recent_tokens: u64) -> CompactionSettings {
    CompactionSettings {
        keep_recent_tokens,
        ..CompactionSettings::default()
    }
}

fn plan(messages: &[ProviderMessage], keep_recent_tokens: u64) -> Option<CompactionPreparation> {
    prepare_compaction(
        messages,
        &settings(keep_recent_tokens),
        ContextUsage::default(),
    )
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
fn compaction_policy_defaults_are_pis_with_slim_persistence_guards() {
    let policy = CompactionPolicy::default();
    assert!(policy.enabled);
    assert_eq!(policy.reserve_tokens, 16_384);
    assert_eq!(policy.keep_recent_tokens, 20_000);
    assert_eq!(policy.summary_max_bytes, 64 * 1024);
    assert_eq!(policy.manual_instructions_max_bytes, 4 * 1024);
    assert_eq!(policy.settings(), CompactionSettings::default());
}

#[test]
fn the_trigger_is_strictly_above_the_window_minus_the_reserve() {
    let settings = CompactionPolicy::default().settings();
    for (window, line) in [
        (32_000, 15_616),
        (128_000, 111_616),
        (200_000, 183_616),
        (1_000_000, 983_616),
    ] {
        assert!(!should_compact(line, window, &settings), "{window}");
        assert!(should_compact(line + 1, window, &settings), "{window}");
    }
    // A window smaller than the reserve compacts at any usage.
    assert!(should_compact(0, 8_000, &settings));
    // A disabled policy never does.
    assert!(!should_compact(
        u64::MAX,
        100,
        &CompactionSettings {
            enabled: false,
            ..settings
        }
    ));
}

#[test]
fn the_cut_keeps_the_recent_tail_and_never_starts_on_a_tool_result() {
    let messages = vec![
        user("literal root"),
        assistant("old"),
        user("x".repeat(80_000)),
        ProviderMessage::assistant("recent", vec![call("call-1", "read", "{}")]),
        ProviderMessage::tool("read", "call-1", "ok"),
    ];
    let plan = plan(&messages, 20_000).expect("compactable");

    // The 20k-token message crosses the budget: it is the first kept message.
    assert_eq!(plan.first_kept_index, 2);
    assert_ne!(messages[plan.first_kept_index].role, "tool");
    assert!(!plan.is_split_turn);
    assert_eq!(plan.messages_to_summarize, messages[..2]);
    assert!(plan.turn_prefix_messages.is_empty());
}

#[test]
fn a_recent_tool_group_is_kept_whole_from_its_call() {
    let messages = vec![
        user("old transcript"),
        ProviderMessage::assistant(
            "working",
            vec![call("call-1", "read", "{}"), call("call-2", "list", "{}")],
        ),
        ProviderMessage::tool("read", "call-1", "one"),
        ProviderMessage::tool("list", "call-2", "two"),
    ];
    let plan = plan(&messages, 1).expect("compactable");

    // The newest message is a result, never a cut point: the cut falls on the
    // assistant message that made the calls.
    assert_eq!(plan.first_kept_index, 1);
    let compacted = apply_compaction(&messages, plan.first_kept_index, "stable summary");
    assert_eq!(compacted[0], compaction_summary_message("stable summary"));
    assert_eq!(compacted[1..], messages[1..]);
    assert_eq!(compacted[2].tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(compacted[3].tool_call_id.as_deref(), Some("call-2"));
}

#[test]
fn the_latest_instruction_is_summarized_like_any_other_closed_work() {
    let mut messages = vec![
        user("root authority"),
        assistant("completed setup"),
        user("latest instruction: preserve this wording exactly"),
    ];
    messages.extend(
        (0..30).map(|index| assistant(format!("closed-work-{index}: {}", "x".repeat(3_000)))),
    );
    let plan = plan(&messages, 20_000).expect("compactable");

    // About 27 closed-work messages fit the budget; the cut lands inside the
    // turn that the latest instruction started, so that turn is split.
    assert_eq!(plan.first_kept_index, 6);
    assert!(plan.is_split_turn);
    assert_eq!(plan.messages_to_summarize, messages[..2]);
    assert_eq!(plan.turn_prefix_messages, messages[2..6]);

    let compacted = apply_compaction(
        &messages,
        plan.first_kept_index,
        "checkpoint for closed work",
    );
    assert_eq!(compacted.len(), 1 + messages.len() - 6);
    assert!(compacted
        .iter()
        .all(|message| message.content != messages[2].content));
}

#[test]
fn an_older_instruction_is_not_pinned_when_the_newest_is_kept() {
    let messages = vec![
        user("root authority"),
        user("older instruction"),
        assistant("closed answer"),
        user("newest instruction kept in suffix"),
    ];
    let plan = plan(&messages, 1).expect("compactable");
    assert_eq!(plan.first_kept_index, 3);
    assert!(!plan.is_split_turn);
    assert_eq!(plan.messages_to_summarize, messages[..3]);
    let compacted = apply_compaction(&messages, plan.first_kept_index, "summary");
    assert_eq!(compacted[1..], messages[3..]);
}

#[test]
fn system_and_developer_messages_are_never_summarized_and_stay_first() {
    let mut system = user("system authority");
    system.role = "system".into();
    let mut developer = user("developer constraint");
    developer.role = "developer".into();
    let messages = vec![
        system.clone(),
        developer.clone(),
        user("root instruction"),
        assistant("closed result"),
        user("latest instruction"),
        ProviderMessage::assistant(
            "active call",
            vec![call("active-1", "read", r#"{"path":"active.txt"}"#)],
        ),
        ProviderMessage::tool("read", "active-1", "active result"),
    ];
    let plan = plan(&messages, 1).expect("compactable");

    assert_eq!(plan.first_kept_index, 5);
    assert!(plan
        .messages_to_summarize
        .iter()
        .chain(&plan.turn_prefix_messages)
        .all(|message| !matches!(message.role.as_str(), "system" | "developer")));
    let compacted = apply_compaction(&messages, plan.first_kept_index, "closed work checkpoint");
    assert_eq!(compacted[0], system);
    assert_eq!(compacted[1], developer);
    assert_eq!(
        compacted[2],
        compaction_summary_message("closed work checkpoint")
    );
    assert_eq!(compacted[3], messages[5]);
    assert_eq!(compacted[4], messages[6]);
    assert_eq!(compacted[4].tool_call_id.as_deref(), Some("active-1"));
}

#[test]
fn nothing_is_compacted_when_the_history_fits_or_just_was() {
    let messages = vec![user("root"), assistant("answer")];
    assert!(plan(&messages, 20_000).is_none());

    let mut compacted = vec![compaction_summary_message("summary")];
    assert!(
        plan(&compacted, 1).is_none(),
        "a summary alone is not history"
    );
    compacted.push(user("x".repeat(100_000)));
    assert!(
        plan(&compacted, 1).is_none(),
        "a single message after the summary has nothing before the cut"
    );
}

#[test]
fn a_split_turn_without_history_carries_its_prefix_summary_alone() {
    let recovery = format!(
        "stale read: t.txt; the precondition differs from current bytes.\nCurrent file is below; retry write with expected set to this full text.\n{}",
        "R".repeat(20_000)
    );
    let messages = vec![
        user("root"),
        ProviderMessage::assistant(
            "overwrite",
            vec![call(
                "write-1",
                "write",
                r#"{"path":"t.txt","content":"next"}"#,
            )],
        ),
        ProviderMessage::tool("write", "write-1", recovery),
        assistant("continue without rereading"),
    ];
    let plan = plan(&messages, 1_000).expect("compactable");

    // A big tool result gets no special retention: it is summarized, and
    // serialized with at most 2000 of its characters.
    assert_eq!(plan.first_kept_index, 3);
    assert!(plan.is_split_turn);
    let requests = plan.summary_requests(None);
    assert!(requests.history.is_none());
    let turn_prefix = requests.turn_prefix.expect("turn prefix request");
    assert!(turn_prefix.prompt.contains("more characters truncated]"));
    assert!(!turn_prefix.prompt.contains(&"R".repeat(2_001)));
    assert_eq!(
        plan.assemble_summary(None, Some("tail")),
        Ok(format!(
            "{NO_PRIOR_HISTORY}\n\n---\n\n**Turn Context (split turn):**\n\ntail"
        ))
    );
    assert_eq!(
        merge_split_turn_summary("history", "tail"),
        "history\n\n---\n\n**Turn Context (split turn):**\n\ntail"
    );
}

#[test]
fn iterative_compaction_updates_the_previous_summary_found_in_the_history() {
    let messages = vec![
        compaction_summary_message("## Goal\nprior"),
        user("task 1"),
        assistant("answer 1"),
        user("task 2 ".repeat(15_000)),
        assistant("ok"),
    ];
    let plan = plan(&messages, 100).expect("compactable");

    assert_eq!(plan.previous_summary.as_deref(), Some("## Goal\nprior"));
    assert_eq!(plan.first_kept_index, 3);
    let requests = plan.summary_requests(Some("preserve the auth constraint"));
    assert!(requests.turn_prefix.is_none());
    let history = requests.history.expect("history request");
    assert!(history
        .prompt
        .contains("<previous-summary>\n## Goal\nprior\n</previous-summary>"));
    assert!(history.prompt.contains("[User]: task 1"));
    assert!(history.prompt.ends_with(&format!(
        "{UPDATE_SUMMARIZATION_PROMPT}\n\nAdditional focus: preserve the auth constraint"
    )));
    // The summary message is not serialized as conversation.
    assert!(!history.prompt.contains(COMPACTION_SUMMARY_PREFIX));
    // The model's own output limit lowers this on the wire request
    // (tests/provider_http.rs).
    assert_eq!(history.max_output_tokens, 13_107);
}

#[test]
fn file_operations_accumulate_across_compactions() {
    let big = || user("x".repeat(100_000));
    let mut messages = vec![
        user("work"),
        ProviderMessage::assistant("", vec![call("c1", "read", r#"{"path":"a.rs"}"#)]),
        ProviderMessage::tool("read", "c1", "a"),
        ProviderMessage::assistant(
            "",
            vec![call("c2", "write", r#"{"path":"b.rs","content":"b"}"#)],
        ),
        ProviderMessage::tool("write", "c2", "ok"),
        ProviderMessage::assistant("", vec![call("c3", "read", r#"{"path":"c.rs"}"#)]),
        ProviderMessage::tool("read", "c3", "c"),
        ProviderMessage::assistant(
            "",
            vec![call("c4", "patch", r#"{"path":"a.rs","edits":[]}"#)],
        ),
        ProviderMessage::tool("patch", "c4", "ok"),
        // The turn ended: a tool batch still awaiting its answer is a live
        // continuation, which a compaction never summarizes.
        ProviderMessage::assistant("done", Vec::new()),
        big(),
    ];
    let first = plan(&messages, 20_000).expect("first compaction");
    assert_eq!(first.first_kept_index, 10);
    let fitted =
        fit_summary_for_persistence("## Goal\nfirst", &first.file_ops, 64 * 1024).expect("fits");
    // Read-only files exclude the modified ones; both lists are sorted.
    assert_eq!(fitted.files.read_files, ["c.rs"]);
    assert_eq!(fitted.files.modified_files, ["a.rs", "b.rs"]);
    assert!(fitted.summary.contains(
        "<read-files>\nc.rs\n</read-files>\n\n<modified-files>\na.rs\nb.rs\n</modified-files>"
    ));

    messages = apply_compaction(&messages, first.first_kept_index, &fitted.summary);
    messages.push(ProviderMessage::assistant(
        "",
        vec![call("c5", "read", r#"{"path":"d.rs"}"#)],
    ));
    messages.push(ProviderMessage::tool("read", "c5", "d"));
    messages.push(ProviderMessage::assistant("done", Vec::new()));
    messages.push(big());
    let second = plan(&messages, 20_000).expect("second compaction");
    let fitted =
        fit_summary_for_persistence("## Goal\nsecond", &second.file_ops, 64 * 1024).expect("fits");
    assert_eq!(fitted.files.read_files, ["c.rs", "d.rs"]);
    assert_eq!(fitted.files.modified_files, ["a.rs", "b.rs"]);
}

#[test]
fn a_summary_over_the_persistence_limit_fails_instead_of_being_shortened() {
    let error = fit_summary_for_persistence(&"x".repeat(100), &FileOperations::new(), 50)
        .expect_err("over the limit");
    assert!(error.contains("persistence limit"), "{error}");
    assert!(fit_summary_for_persistence("fits", &FileOperations::new(), 50).is_ok());
}

#[test]
fn the_summary_message_wraps_the_text_the_way_pi_does() {
    let message = compaction_summary_message("## Goal\nship it");
    assert_eq!(message.role, "user");
    assert_eq!(
        message.content,
        format!("{COMPACTION_SUMMARY_PREFIX}## Goal\nship it{COMPACTION_SUMMARY_SUFFIX}")
    );
    assert_eq!(compaction_summary_text(&message), Some("## Goal\nship it"));
    assert_eq!(compaction_summary_text(&user("plain")), None);
}

#[test]
fn summary_prompts_read_text_blocks_and_estimates_cap_attachments() {
    let image = assistant("image").with_content_blocks(vec![ProviderContentBlock::Image {
        media_type: "image/png".into(),
        data: "aW1hZ2U=".into(),
    }]);
    let text_block =
        assistant("old").with_content_blocks(vec![ProviderContentBlock::text("text-block fact")]);
    let request = build_history_summary_request([&text_block, &image], 16_384, None, None);

    assert!(request.prompt.contains("text-block fact"));
    // An image counts 4800 characters however large its data is.
    assert_eq!(estimate_tokens(&image), 1_202);
}

#[test]
fn manual_instructions_are_bounded_and_commits_are_taken_once() {
    let handle = CompactionHandle::default();
    handle.request_manual("focus").expect("manual request");
    assert_eq!(handle.manual_instructions().as_deref(), Some("focus"));
    assert!(handle.request_manual("x".repeat(4 * 1024 + 1)).is_err());
    assert_eq!(
        handle.manual_instructions().as_deref(),
        Some("focus"),
        "a rejected request keeps the pending one"
    );
    handle.clear_manual();
    assert!(handle.manual_instructions().is_none());

    assert_eq!(handle.status(), CompactionStatus::Idle);
    handle.commit_detailed(CompactionCommit {
        summary: "summary".into(),
        canonical_prefix_fingerprint: "canonical".into(),
        first_kept_index: 2,
        tokens_before: 100,
        tokens_after: 20,
        input_tokens: 10,
        output_tokens: 2,
        duration_ms: 1,
        reason: CompactionReason::Threshold,
        read_files: vec!["a.rs".into()],
        modified_files: vec!["b.rs".into()],
    });
    assert_eq!(handle.status(), CompactionStatus::Applied);
    let commits = handle.take_commits();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].summary, "summary");
    assert_eq!(commits[0].read_files, ["a.rs"]);
    assert!(handle.take_commits().is_empty());
}

#[test]
fn legacy_threshold_reasons_load_as_the_single_threshold_reason() {
    let parse = |text: &str| serde_json::from_str::<CompactionReason>(text).expect(text);
    assert_eq!(parse("\"threshold\""), CompactionReason::Threshold);
    assert_eq!(parse("\"soft_threshold\""), CompactionReason::Threshold);
    assert_eq!(parse("\"hard_threshold\""), CompactionReason::Threshold);
    assert_eq!(parse("\"manual\""), CompactionReason::Manual);
    assert_eq!(
        serde_json::to_string(&CompactionReason::Threshold).expect("serialize"),
        "\"threshold\""
    );
}

#[test]
fn compaction_fingerprint_covers_name_and_content_blocks() {
    let original = user("content").with_content_blocks(vec![ProviderContentBlock::text("alpha")]);
    let mut renamed = original.clone();
    renamed.name = Some("renamed".into());
    let changed_block =
        user("content").with_content_blocks(vec![ProviderContentBlock::text("beta")]);

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
fn adaptive_estimator_does_not_add_overhead_already_present_on_the_wire() {
    let mut estimator = AdaptiveTokenEstimator::default();
    assert_eq!(estimator.estimate("provider", "model", 350), 100);
    for _ in 0..4 {
        estimator.observe("provider", "model", 350, 100);
    }
    assert_eq!(estimator.estimate("provider", "model", 350), 100);
}
