use slim_core::mcp::{authorize_http, canonical_name, JsonLineFramer, McpCatalog, McpLifecycle};
use slim_core::OperatingMode;

#[test]
fn fragmented_stdio_frames_reassemble_without_loss() {
    let mut framer = JsonLineFramer::default();
    assert!(framer
        .push(br#"{"jsonrpc":"2.0","id":1"#)
        .expect("push")
        .is_empty());
    let messages = framer
        .push(b", \"result\": {\"ok\": true}}\n")
        .expect("push");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["result"]["ok"], true);
}

#[test]
fn catalog_namespaces_tools_and_requires_explicit_resource_selection() {
    let mut catalog = McpCatalog::new("server");
    catalog.add_tool("read");
    catalog.add_resource("file://one");
    catalog.add_prompt("summarize");
    assert_eq!(canonical_name("server", "read"), "mcp.server.read");
    assert!(catalog
        .tools_for_mode(OperatingMode::Auto)
        .contains(&"mcp.server.read".to_owned()));
    assert!(catalog.tools_for_mode(OperatingMode::Plan).is_empty());
    assert!(catalog.selected_resources().is_empty());
    catalog.select_resource("file://one").expect("select");
    assert_eq!(catalog.selected_resources(), &["file://one".to_owned()]);
    catalog.select_prompt("summarize").expect("prompt");
    assert_eq!(catalog.selected_prompts(), &["summarize".to_owned()]);
    assert_eq!(
        catalog
            .call_tool(OperatingMode::Auto, "read", "{}")
            .expect("tool"),
        "mcp.server.read({})"
    );
    assert!(catalog
        .call_tool(OperatingMode::Plan, "read", "{}")
        .is_err());
}

#[test]
fn static_http_auth_and_idempotent_lifecycle_are_deterministic() {
    assert!(authorize_http("Bearer token", "token"));
    assert!(!authorize_http("Bearer other", "token"));
    let mut lifecycle = McpLifecycle::new();
    assert!(lifecycle.is_running());
    lifecycle.cancel();
    lifecycle.cancel();
    assert!(!lifecycle.is_running());
}
