use super::{attach_local_mcp, ProviderRunOptions};
use slim_core::mcp::McpServerStatus;
use std::fs;
use std::path::PathBuf;

fn workspace(label: &str, slim_toml: Option<&str>) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "slim-attach-mcp-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("workspace");
    if let Some(contents) = slim_toml {
        fs::write(root.join("slim.toml"), contents).expect("project config");
    }
    root
}

fn status_of(manager: &slim_core::mcp::McpManager, name: &str) -> McpServerStatus {
    manager
        .statuses()
        .into_iter()
        .find(|info| info.name == name)
        .unwrap_or_else(|| panic!("server {name} listed"))
        .status
}

#[test]
fn project_servers_are_untrusted_by_default_and_reported_with_the_flag_hint() {
    let root = workspace(
        "untrusted",
        Some("[mcp.servers.proj]\ncommand = \"definitely-not-a-real-binary\"\n"),
    );
    let mut options = ProviderRunOptions::default().with_workspace_root(&root);
    let (manager, warnings) = attach_local_mcp(&mut options);
    let manager = manager.expect("manager built from the workspace project file");
    assert!(options.mcp.is_some());
    assert!(matches!(
        status_of(&manager, "proj"),
        McpServerStatus::Untrusted
    ));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("proj") && warnings[0].contains("--trust-project"),
        "{warnings:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn trust_project_flag_starts_project_servers_for_this_run_without_warnings() {
    let root = workspace(
        "trusted",
        Some("[mcp.servers.proj]\ncommand = \"definitely-not-a-real-binary\"\n"),
    );
    let mut options = ProviderRunOptions::default()
        .with_workspace_root(&root)
        .with_trust_project(true);
    let (manager, warnings) = attach_local_mcp(&mut options);
    let manager = manager.expect("manager");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(matches!(
        status_of(&manager, "proj"),
        McpServerStatus::Disconnected
    ));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn invalid_config_disables_mcp_with_a_warning_instead_of_vanishing() {
    let root = workspace(
        "invalid",
        Some("[mcp.servers.bad]\ncommand = \"x\"\nurl = \"https://x\"\n"),
    );
    let mut options = ProviderRunOptions::default().with_workspace_root(&root);
    let (manager, warnings) = attach_local_mcp(&mut options);
    assert!(manager.is_none() && options.mcp.is_none());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("MCP disabled") && warnings[0].contains("not both"),
        "{warnings:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn no_project_file_means_no_manager_and_no_warning() {
    let root = workspace("none", None);
    let mut options = ProviderRunOptions::default().with_workspace_root(&root);
    let (manager, warnings) = attach_local_mcp(&mut options);
    assert!(manager.is_none() && warnings.is_empty());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_existing_handle_is_left_alone() {
    let root = workspace("existing", Some("[mcp.servers.proj]\ncommand = \"x\"\n"));
    let manager = std::sync::Arc::new(slim_core::mcp::McpManager::new(
        Default::default(),
        root.clone(),
        Default::default(),
    ));
    let mut options = ProviderRunOptions::default().with_workspace_root(&root);
    options.mcp = Some(super::McpHandle::new(manager));
    let (attached, warnings) = attach_local_mcp(&mut options);
    assert!(attached.is_none() && warnings.is_empty());
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_connects_non_lazy_servers_in_the_background_and_leaves_lazy_ones() {
    let root = workspace(
        "background",
        Some(
            "[mcp.servers.eager]\ncommand = \"definitely-not-a-real-binary\"\n[mcp.servers.lazy]\ncommand = \"definitely-not-a-real-binary\"\nlazy = true\n",
        ),
    );
    let mut options = ProviderRunOptions::default()
        .with_workspace_root(&root)
        .with_trust_project(true);
    let (manager, warnings) = attach_local_mcp(&mut options);
    let manager = manager.expect("manager");
    assert!(warnings.is_empty(), "{warnings:?}");
    // The eager server's attempt (it cannot start) happens without anyone
    // asking for it; the lazy one waits for its first use.
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !matches!(status_of(&manager, "eager"), McpServerStatus::Failed { .. }) {
        assert!(
            std::time::Instant::now() < end,
            "no background connect attempt"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(matches!(
        status_of(&manager, "lazy"),
        McpServerStatus::Disconnected
    ));
    manager.disconnect_all().await;
    let _ = fs::remove_dir_all(&root);
}
