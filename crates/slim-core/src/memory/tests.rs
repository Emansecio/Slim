use std::fs;
use std::path::Path;
use std::sync::MutexGuard;

use super::*;
use crate::checkpoint::tests::SLIM_HOME_LOCK;
use crate::runtime::scratchpad::SessionScratchpad;

fn lock_slim_home(path: &Path) -> MutexGuard<'static, ()> {
    let guard = SLIM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("SLIM_HOME", path);
    guard
}

#[test]
fn memory_round_trip_dedupes_upgrades_and_removes() {
    let root = std::env::temp_dir().join(format!("slim-mem-rt-{}", std::process::id()));
    let slim_home = root.join(".slim-home");
    let proj = root.join("proj");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&proj).unwrap();
    let _guard = lock_slim_home(&slim_home);

    let first = add_memory_entry(
        &proj,
        MemoryKind::Command,
        "  cargo test  --lib  ",
        MemorySource::Auto,
    )
    .expect("add auto entry");
    assert_eq!(first.text, "cargo test --lib");
    assert_eq!(first.hits, 1);
    assert_eq!(first.source, MemorySource::Auto);

    let second = add_memory_entry(
        &proj,
        MemoryKind::Command,
        "CARGO TEST --LIB",
        MemorySource::User,
    )
    .expect("dedupe & upgrade to user");
    assert_eq!(second.id, first.id);
    assert_eq!(second.hits, 2);
    assert_eq!(second.source, MemorySource::User);
    assert_eq!(second.text, "CARGO TEST --LIB");

    let loaded = load_project_memory(&proj);
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].hits, 2);

    let prefix = &first.id[..4];
    let removed = remove_memory_entry(&proj, prefix).expect("remove by prefix");
    assert_eq!(removed.id, first.id);
    assert!(load_project_memory(&proj).entries.is_empty());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn lru_eviction_preserves_user_entries_over_auto_at_capacity() {
    let root = std::env::temp_dir().join(format!("slim-mem-lru-{}", std::process::id()));
    let slim_home = root.join(".slim-home");
    let proj = root.join("proj");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&proj).unwrap();
    let _guard = lock_slim_home(&slim_home);

    let pinned_user = add_memory_entry(
        &proj,
        MemoryKind::Convention,
        "Sempre usar edition 2021 em slim-core",
        MemorySource::User,
    )
    .expect("user entry");

    for idx in 0..MAX_MEMORY_ENTRIES {
        add_memory_entry(
            &proj,
            MemoryKind::Command,
            &format!("cargo test case_{idx}"),
            MemorySource::Auto,
        )
        .expect("auto entry");
    }

    let mem = load_project_memory(&proj);
    assert_eq!(mem.entries.len(), MAX_MEMORY_ENTRIES);
    assert!(
        mem.entries.iter().any(|e| e.id == pinned_user.id),
        "user entry must never be evicted while auto entries exist"
    );

    let cleared = clear_project_memory(&proj).expect("clear");
    assert_eq!(cleared, MAX_MEMORY_ENTRIES);
    assert!(load_project_memory(&proj).entries.is_empty());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn render_is_deterministic_prioritizes_user_and_respects_byte_budget() {
    let root = std::env::temp_dir().join(format!("slim-mem-ren-{}", std::process::id()));
    let slim_home = root.join(".slim-home");
    let proj = root.join("proj");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&proj).unwrap();
    let _guard = lock_slim_home(&slim_home);

    add_memory_entry(&proj, MemoryKind::Gotcha, "Cuidado com X", MemorySource::Auto).unwrap();
    add_memory_entry(&proj, MemoryKind::Convention, "Regra explícita do usuário", MemorySource::User)
        .unwrap();
    for idx in 0..15 {
        let long_cmd = format!("cargo test long_command_{idx}_{}", "x".repeat(180));
        add_memory_entry(&proj, MemoryKind::Command, &long_cmd, MemorySource::Auto).unwrap();
    }

    let r1 = render_for_prompt(&proj).expect("rendered");
    let r2 = render_for_prompt(&proj).expect("rendered second");
    assert_eq!(r1, r2, "rendering must be byte-identical");
    assert!(r1.len() <= MAX_RENDERED_MEMORY_BYTES);
    assert!(
        r1.starts_with("- [convention] Regra explícita do usuário"),
        "user entry should appear before auto entries: {r1}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn persist_scratchpad_learnings_filters_secrets_and_multiline_commands() {
    let root = std::env::temp_dir().join(format!("slim-mem-ext-{}", std::process::id()));
    let slim_home = root.join(".slim-home");
    let proj = root.join("proj");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&proj).unwrap();
    let _guard = lock_slim_home(&slim_home);

    let mut scratchpad = SessionScratchpad::default();
    scratchpad.verified_commands = vec![
        String::new(),
        "cargo test -p slim-core --lib memory::tests".to_owned(),
        "line1\nline2".to_owned(),
        "API_KEY=secret123 cargo test".to_owned(),
        "curl -H 'Authorization: Bearer abc123' https://example.com".to_owned(),
        "PASSWORD=hunter2 ./run.sh".to_owned(),
        "TOKEN=tok123 npm test".to_owned(),
        "SECRET=xyz pytest".to_owned(),
    ];

    persist_scratchpad_learnings(&proj, &scratchpad);
    let mem = load_project_memory(&proj);
    assert_eq!(mem.entries.len(), 1);
    assert_eq!(mem.entries[0].kind, MemoryKind::Command);
    assert_eq!(
        mem.entries[0].text,
        "cargo test -p slim-core --lib memory::tests"
    );

    let _ = fs::remove_dir_all(&root);
}
