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
    let registry = ToolRegistry::default();
    for text in ["alpha and beta\n", "alpha and beta\r\n", "alpha and beta"] {
        fs::write(root.join("both.txt"), text).expect("write");
        let result = registry.execute(
            OperatingMode::ReadOnly,
            &root,
            "search",
            r#"{"patterns":["alpha","beta"],"path":"."}"#,
        );
        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("[pattern 1: alpha"));
        assert!(result.output.contains("pattern 2: beta"));
        assert_eq!(
            result.output.split("\n[skipped:").next().expect("hits"),
            "[both.txt]\n[pattern 1: alpha | pattern 2: beta] 1: alpha and beta"
        );
    }
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn repeated_search_patterns_use_a_self_contained_legend_on_each_page() {
    let root = temp_root("pattern-legend");
    fs::create_dir_all(&root).expect("root");
    let patterns = ["reconstruir_contexto_ação", "persistir_checkpoint_seguro"];
    let line = patterns.join(" + ");
    fs::write(root.join("both.txt"), format!("{line}\n").repeat(20)).expect("write");
    let registry = ToolRegistry::default();
    let mut cursor = None;
    for page in 0..2 {
        let result = registry.execute(
            OperatingMode::ReadOnly,
            &root,
            "search",
            &serde_json::json!({"path": ".", "patterns": patterns, "max_hits": 20, "cursor": cursor.clone().unwrap_or_default()}).to_string(),
        );
        assert!(result.success, "{}", result.output);
        let admission = if page == 0 {
            "[admission: search cursor blank omitted]\n"
        } else {
            ""
        };
        let mut expected = format!(
            "{admission}[pattern 1: {}]\n[pattern 2: {}]\n[both.txt]\n",
            patterns[0], patterns[1]
        );
        let mut before = admission.to_owned();
        for number in page * 10 + 1..=page * 10 + 10 {
            expected.push_str(&format!("[pattern 1|2] {number}: {line}\n"));
            for (index, pattern) in patterns.iter().enumerate() {
                before.push_str(&format!(
                    "[pattern {}: {pattern}] both.txt:{number}: {line}\n",
                    index + 1
                ));
            }
        }
        assert!(result.output.starts_with(&expected), "{}", result.output);
        before.push_str(&result.output[expected.len()..]);
        assert!(result.output.len() < before.len());
        assert!(result.output.ends_with("[skipped: node_modules, target, dist, .git, .slim, .pi, .venv — use read/list/shell in those trees]"));
        println!(
            "search legend only page {}: before={} bytes after={} bytes",
            page + 1,
            before.len(),
            result.output.len()
        );
        if page == 0 {
            assert!(result.output.contains("showing hits 1-20 of 40"));
            cursor = Some(cursor_from(&result.output));
            fs::write(root.join("both.txt"), "changed after snapshot\n").expect("change");
        } else {
            assert!(!result.output.contains("cursor"));
        }
    }
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn multipattern_search_reserves_coverage_for_late_pattern() {
    let root = temp_root("pattern-fairness");
    fs::create_dir_all(&root).expect("root");
    let body = format!("{}RARE_ONLY\n", "COMMON\n".repeat(600));
    fs::write(root.join("data.txt"), body).expect("fixture");
    let result = ToolRegistry::default().execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"path":"data.txt","patterns":["COMMON","RARE_ONLY"],"max_hits":500}"#,
    );
    assert!(result.success, "{}", result.output);
    assert!(
        result.output.contains("601: RARE_ONLY"),
        "{}",
        result.output
    );
    assert!(
        result.output.contains("pattern 1 `COMMON`: 499 retained"),
        "{}",
        result.output
    );
    assert!(
        result.output.contains("pattern 2 `RARE_ONLY`: 1 retained"),
        "{}",
        result.output
    );
    assert!(result.output.contains("snapshot capped at 500 hits"));
    assert!(result.output.len() < 64 * 1024);
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn multipattern_search_distinguishes_absence_and_cursor_continuation() {
    let root = temp_root("pattern-coverage");
    fs::create_dir_all(&root).expect("root");
    fs::write(root.join("data.txt"), "COMMON\n".repeat(600)).expect("fixture");
    let registry = ToolRegistry::default();
    let absent = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"path":"data.txt","patterns":["COMMON","MISSING"],"max_hits":500}"#,
    );
    assert!(absent.success, "{}", absent.output);
    assert!(
        absent
            .output
            .contains("pattern 2 `MISSING`: not found after full scan"),
        "{}",
        absent.output
    );
    assert!(!absent.output.contains("pattern 2 `MISSING`: not covered"));

    let body = format!("{}RARE_ONLY\n", "COMMON\n".repeat(600));
    fs::write(root.join("data.txt"), body).expect("fixture");
    let first = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"path":"data.txt","patterns":["COMMON","RARE_ONLY"],"max_hits":499}"#,
    );
    assert!(first.success, "{}", first.output);
    assert!(!first.output.contains("601: RARE_ONLY"));
    let cursor = cursor_from(&first.output);
    let second = registry.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        &serde_json::json!({
            "path": "data.txt",
            "patterns": ["COMMON", "RARE_ONLY"],
            "max_hits": 499,
            "cursor": cursor,
        })
        .to_string(),
    );
    assert!(second.success, "{}", second.output);
    assert!(
        second.output.contains("601: RARE_ONLY"),
        "{}",
        second.output
    );
    assert!(second.output.contains("pattern 2 `RARE_ONLY`: 1 retained"));
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}

#[test]
fn multipattern_search_marks_uncovered_pattern_when_work_budget_ends() {
    let root = temp_root("pattern-work-budget");
    fs::create_dir_all(&root).expect("root");
    for index in 0..4097 {
        fs::write(root.join(format!("file-{index:04}.txt")), "COMMON\n").expect("fixture");
    }
    let result = ToolRegistry::default().execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"path":".","patterns":["COMMON","MISSING"],"max_hits":500}"#,
    );
    assert!(result.success, "{}", result.output);
    assert!(
        result
            .output
            .contains("pattern 2 `MISSING`: not covered; search work budget exhausted"),
        "{}",
        result.output
    );
    assert!(result
        .output
        .contains("uncovered patterns are not confirmed absent"));
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
fn lost_list_and_search_snapshots_require_explicit_restart() {
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
    // The next offset would refer to a different hit after this external edit.
    fs::remove_file(root.join("f-0.txt")).expect("remove first hit");
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
    assert!(!next_search.success, "{}", next_search.output);
    assert!(next_search.output.contains("start a new search"));
    let restarted = consumer.execute(
        OperatingMode::ReadOnly,
        &root,
        "search",
        r#"{"query":"needle","path":"."}"#,
    );
    assert!(restarted.success, "{}", restarted.output);
    assert!(restarted.output.contains("[f-1.txt]\n1: needle"));
    assert!(restarted.output.contains("[f-2.txt]\n1: needle"));
    let _ = fs::remove_dir_all(root.parent().expect("parent"));
}
