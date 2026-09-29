use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use slim_core::provider::ProviderContentBlock;
use slim_tui::api::UiEvent;

use super::{
    load_prompt_mentions, serve_workspace_files, MENTION_FILE_BYTES, MENTION_MAX_FILES,
    MENTION_TOTAL_BYTES,
};

fn workspace(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "slim-mention-{label}-{}-{}",
        std::process::id(),
        super::system_time_nanos(std::time::SystemTime::now())
    ));
    fs::create_dir_all(root.join("src")).expect("workspace");
    root
}

fn block_text(block: &ProviderContentBlock) -> &str {
    match block {
        ProviderContentBlock::Text(text) => text,
        other => panic!("expected text block, got {other:?}"),
    }
}

#[test]
fn attaches_mentioned_files_and_leaves_plain_at_signs_alone() {
    let root = workspace("attach");
    fs::write(root.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }\n").unwrap();
    let attachments = load_prompt_mentions(
        &root,
        "explique @src/lib.rs, use @override e mande para a@b.com",
    )
    .expect("mentions load");
    assert_eq!(attachments.blocks.len(), 1);
    let text = block_text(&attachments.blocks[0]);
    assert!(
        text.contains("src/lib.rs") && text.contains("pub fn answer"),
        "{text}"
    );
    assert_eq!(attachments.labels.len(), 1);
    assert!(
        attachments.labels[0].starts_with("[arquivo · src/lib.rs · "),
        "{:?}",
        attachments.labels
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn prompt_without_mentions_attaches_nothing() {
    let root = workspace("plain");
    let attachments = load_prompt_mentions(&root, "sem arquivos aqui").expect("ok");
    assert!(attachments.blocks.is_empty() && attachments.labels.is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn secret_looking_file_fails_the_whole_prompt() {
    let root = workspace("secret");
    fs::write(root.join(".env"), "TOKEN=abc\n").unwrap();
    fs::write(root.join("src/ok.rs"), "fn ok() {}\n").unwrap();
    let error =
        load_prompt_mentions(&root, "veja @src/ok.rs e @.env").expect_err("secret must be refused");
    assert!(
        error.contains("@.env") && error.contains("segredos"),
        "{error}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn binary_file_is_refused() {
    let root = workspace("binary");
    fs::write(root.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    let error = load_prompt_mentions(&root, "@blob.bin").expect_err("binary refused");
    assert!(error.contains("não é um arquivo de texto"), "{error}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn escaping_paths_never_attach_anything() {
    let root = workspace("escape");
    let outside = root
        .parent()
        .unwrap()
        .join(format!("slim-mention-outside-{}.txt", std::process::id()));
    fs::write(&outside, "segredo de fora").unwrap();
    let relative = format!("../{}", outside.file_name().unwrap().to_string_lossy());
    let attachments = load_prompt_mentions(&root, &format!("leia @{relative}")).expect("ok");
    assert!(
        attachments.blocks.is_empty(),
        "a path outside the workspace must not be read"
    );
    let _ = fs::remove_file(outside);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn too_many_files_are_refused() {
    let root = workspace("many");
    let mut prompt = String::new();
    for index in 0..=MENTION_MAX_FILES {
        fs::write(root.join(format!("f{index}.txt")), "x").unwrap();
        prompt.push_str(&format!("@f{index}.txt "));
    }
    let error = load_prompt_mentions(&root, &prompt).expect_err("limit");
    assert!(error.contains("Máximo"), "{error}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn large_file_is_truncated_and_labelled() {
    let root = workspace("large");
    fs::write(root.join("big.txt"), "a".repeat(MENTION_FILE_BYTES + 10)).unwrap();
    let attachments = load_prompt_mentions(&root, "@big.txt").expect("ok");
    assert!(
        attachments.labels[0].contains("truncado"),
        "{:?}",
        attachments.labels
    );
    assert!(block_text(&attachments.blocks[0]).contains("truncado"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn total_budget_is_enforced_across_files() {
    let root = workspace("total");
    let files = MENTION_TOTAL_BYTES / MENTION_FILE_BYTES + 1;
    assert!(
        files <= MENTION_MAX_FILES,
        "fixture must stay under the count limit"
    );
    let mut prompt = String::new();
    for index in 0..files {
        fs::write(
            root.join(format!("f{index}.txt")),
            "a".repeat(MENTION_FILE_BYTES),
        )
        .unwrap();
        prompt.push_str(&format!("@f{index}.txt "));
    }
    let error = load_prompt_mentions(&root, &prompt).expect_err("total budget");
    assert!(error.contains("Limite"), "{error}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_file_request_is_answered_with_the_same_id() {
    let root = workspace("list");
    fs::write(root.join("README.md"), "r").unwrap();
    fs::write(root.join("src/lib.rs"), "l").unwrap();
    let (sink, control_rx, _data_rx) = super::cancel_tests::sink_for_tests();
    serve_workspace_files(Some(root.clone()), &sink, 41);
    match control_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("answer arrives")
    {
        UiEvent::WorkspaceFiles {
            request_id,
            paths,
            truncated,
        } => {
            assert_eq!(request_id, 41);
            assert!(!truncated);
            assert_eq!(paths, ["README.md", "src/lib.rs"]);
        }
        other => panic!("unexpected event {other:?}"),
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn unreadable_workspace_still_answers_so_the_popup_does_not_wait() {
    let missing = std::env::temp_dir().join(format!("slim-mention-missing-{}", std::process::id()));
    let (sink, control_rx, _data_rx) = super::cancel_tests::sink_for_tests();
    serve_workspace_files(Some(missing), &sink, 7);
    let mut answered = false;
    for _ in 0..2 {
        match control_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("event")
        {
            UiEvent::Notification { .. } => {}
            UiEvent::WorkspaceFiles {
                request_id, paths, ..
            } => {
                assert_eq!(request_id, 7);
                assert!(paths.is_empty());
                answered = true;
                break;
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert!(answered);
}
