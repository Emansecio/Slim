use std::fs;

use slim_core::tools::{search_bounded, SearchOptions, ToolRegistry, DEFAULT_MAX_HITS};
use slim_core::OperatingMode;

fn temp_root(name: &str) -> std::path::PathBuf {
    let unique = format!(
        "slim-search-bounded-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    std::env::temp_dir().join(unique).join(name)
}

fn cursor_from(output: &str) -> String {
    let marker = "pass \"cursor\": \"";
    let start = output.find(marker).expect("cursor footer") + marker.len();
    let rest = &output[start..];
    rest[..rest.find('"').expect("cursor end")].to_owned()
}

#[test]
fn registry_search_honors_max_hits_and_offset() {
    let root = temp_root("registry");
    fs::create_dir_all(root.join("src")).expect("src");
    for index in 0..5 {
        fs::write(
            root.join("src").join(format!("f-{index}.txt")),
            format!("token-{index}"),
        )
        .expect("write");
    }
    let registry = ToolRegistry::default();
    let first = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"query":"token","path":"src","max_hits":2,"offset":1}"#,
    );
    assert!(first.success);
    assert_eq!(
        first
            .output
            .lines()
            .filter(|line| line.contains("token-"))
            .count(),
        2
    );
    assert!(first.output.contains("[showing hits"));
    let cursor = cursor_from(&first.output);
    for entry in fs::read_dir(root.join("src")).expect("entries") {
        fs::remove_file(entry.expect("entry").path()).expect("remove source after snapshot");
    }

    let arguments = serde_json::json!({
        "query": "token",
        "path": "src",
        "max_hits": 2,
        "cursor": cursor,
    })
    .to_string();
    let second = registry.execute(OperatingMode::ReadOnly, &root, "search", &arguments);
    assert!(second.success);
    assert_eq!(
        second
            .output
            .lines()
            .filter(|line| line.contains("token-"))
            .count(),
        2
    );
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn registry_search_accepts_multiple_patterns_and_identifies_each_hit() {
    let root = temp_root("patterns");
    fs::create_dir_all(&root).expect("root");
    fs::write(root.join("both.txt"), "alpha and beta\n").expect("write");
    let registry = ToolRegistry::default();

    let result = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"patterns":["alpha","beta"],"path":"."}"#,
    );

    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("[pattern 1: alpha]"));
    assert!(result.output.contains("[pattern 2: beta]"));
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn search_in_repo_with_dist_does_not_materialize_large_artifact() {
    let root = temp_root("artifact");
    fs::create_dir_all(root.join("src")).expect("src");
    fs::create_dir_all(root.join("dist")).expect("dist");
    fs::write(root.join("src/ok.ts"), "onboarding").expect("src");
    fs::write(root.join("dist/bundle.js"), "onboarding\n".repeat(20_000)).expect("dist");

    let page = search_bounded(SearchOptions {
        query: "onboarding".into(),
        root: root.clone(),
        offset: 1,
        max_hits: DEFAULT_MAX_HITS,
    })
    .expect("search");
    assert_eq!(page.hits.len(), 1);
    assert!(page.hits[0].path.ends_with("src/ok.ts"));

    let registry = ToolRegistry::default();
    let result = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"query":"onboarding","path":"."}"#,
    );
    assert!(result.success);
    assert!(result.output.len() < 64 * 1024);
    assert!(result.output.contains("skipped: node_modules, target"));
    assert!(result.artifact.is_none());
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn list_snapshot_is_not_rebuilt_while_search_can_recover_after_registry_cache_loss() {
    let root = temp_root("cursor-recovery");
    fs::create_dir_all(&root).expect("root");
    for index in 0..3 {
        fs::write(root.join(format!("f-{index}.txt")), "needle\n").expect("fixture");
    }

    let producer = ToolRegistry::default();
    let first_list = producer.execute(
        OperatingMode::ReadOnly,
        &root,
        "list",
        r#"{"path":".","max_entries":1}"#,
    );
    let first_search = producer.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"query":"needle","path":".","max_hits":1}"#,
    );
    let list_cursor = cursor_from(&first_list.output);
    let search_cursor = cursor_from(&first_search.output);

    let consumer = ToolRegistry::default();
    let next_list = consumer.execute(
        OperatingMode::ReadOnly,
        &root,
        "list",
        &serde_json::json!({"path": ".", "max_entries": 1, "cursor": list_cursor}).to_string(),
    );
    let next_search = consumer.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        &serde_json::json!({
            "query": "needle",
            "path": ".",
            "max_hits": 1,
            "cursor": search_cursor
        })
        .to_string(),
    );

    assert!(!next_list.success);
    assert!(next_list.output.contains("start a new list request"));
    assert!(next_search.success, "{}", next_search.output);
    assert!(next_search.output.contains("showing hits 2-2"));
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}
