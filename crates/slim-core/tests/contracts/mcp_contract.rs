use slim_core::mcp::{canonical_name, FramedLine, JsonLineFramer, McpCatalog};
use slim_core::OperatingMode;

#[test]
fn fragmented_stdio_frames_reassemble_without_loss() {
    let mut framer = JsonLineFramer::default();
    assert!(framer.push(br#"{"jsonrpc":"2.0","id":1"#).is_empty());
    let lines = framer.push(b", \"result\": {\"ok\": true}}\n");
    assert_eq!(lines.len(), 1);
    let FramedLine::Message(message) = &lines[0] else {
        panic!("expected message, got {lines:?}");
    };
    assert_eq!(message["result"]["ok"], true);
}

#[test]
fn non_json_stdout_lines_become_noise_not_fatal() {
    let mut framer = JsonLineFramer::default();
    let lines = framer.push(b"server log line\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n");
    assert_eq!(lines.len(), 2);
    assert!(matches!(&lines[0], FramedLine::Noise(noise) if noise == "server log line"));
    assert!(matches!(&lines[1], FramedLine::Message(_)));
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
