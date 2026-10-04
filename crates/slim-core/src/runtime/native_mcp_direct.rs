//! MCP tools exposed as provider tools (`exposure = "direct"`): declaration
//! next to the native tools, and execution through the same manager path as
//! the `mcp` gateway (cancellation, no-replay classification, redaction,
//! budgets and progress all come from there).

use super::native_mcp::render::{render_call_tool_result, RenderEnv, Rendered};
use super::native_mcp::run_mcp_dispatch_with;
use super::*;
use crate::context::ArtifactHandle;
use crate::mcp::{is_direct_tool_name, McpProgressSink};

/// Whether a provider tool call targets a direct MCP tool.
pub(super) fn is_direct_mcp_call(name: &str) -> bool {
    is_direct_tool_name(name)
}

/// The tool result of a finished MCP request, with the artifacts created for
/// its binary parts. `name` is the name the model called (`mcp` or
/// `mcp__<server>__<tool>`). Both entry points render through the same
/// pipeline, so media and artifacts behave alike.
pub(super) fn mcp_outcome_result(
    name: &str,
    outcome: McpRequestOutcome<Rendered>,
) -> (ToolResult, Vec<ArtifactHandle>) {
    let fail = |output: String| (ToolResult::new(name, false, output), Vec::new());
    match outcome {
        McpRequestOutcome::Completed(Ok(rendered)) => {
            let mut result = ToolResult::new(name, rendered.success, rendered.text);
            result.media = rendered.media;
            (result, rendered.blobs)
        }
        McpRequestOutcome::Completed(Err(error)) => fail(format!("mcp error: {error}")),
        McpRequestOutcome::InterruptedBeforeSend {
            interruption,
            cleanup,
        } => fail(format!("mcp operation stopped before tools/call was sent: {interruption:?}; transport cleanup: {cleanup:?}")),
        McpRequestOutcome::OutcomeUncertain { interruption, cleanup } => fail(format!(
            "mcp operation outcome is uncertain: {interruption:?}; transport cleanup: {cleanup:?}; do not replay the call automatically"
        )),
    }
}

/// Routes a call to the gateway or to a direct tool by the called name.
pub(super) async fn run_mcp_route(
    manager: Option<Arc<McpManager>>,
    name: &str,
    arguments: &str,
    cancellation: McpCancellation,
    progress: Option<McpProgressSink>,
    env: &RenderEnv<'_>,
) -> (ToolResult, Vec<ArtifactHandle>) {
    if is_direct_mcp_call(name) {
        run_mcp_direct_dispatch(manager, name, arguments, cancellation, progress, env).await
    } else {
        run_mcp_dispatch_with(manager, arguments, cancellation, progress, env).await
    }
}

/// Arguments of a direct call: the model's JSON object, `{}` when empty.
fn direct_arguments(raw: &str) -> Result<serde_json::Map<String, Value>, String> {
    if raw.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(arguments)) => Ok(arguments),
        Ok(Value::Null) => Ok(serde_json::Map::new()),
        Ok(_) => Err("invalid MCP tool arguments: expected a JSON object".into()),
        Err(error) => Err(format!("invalid MCP tool arguments: {error}")),
    }
}

async fn run_mcp_direct_dispatch(
    manager: Option<Arc<McpManager>>,
    name: &str,
    arguments: &str,
    cancellation: McpCancellation,
    progress: Option<McpProgressSink>,
    env: &RenderEnv<'_>,
) -> (ToolResult, Vec<ArtifactHandle>) {
    let fail = |message: String| (ToolResult::new(name, false, message), Vec::new());
    let Some(manager) = manager else {
        return fail(
            "mcp unavailable: no MCP servers configured (set [mcp.servers] in slim.toml)".into(),
        );
    };
    let Some((server, tool)) = manager.resolve_direct_tool(name) else {
        return fail(format!(
            "unknown MCP tool: {}; it is not a direct tool of an enabled server (use the mcp tool to list and call tools)",
            name.chars().take(64).collect::<String>()
        ));
    };
    let arguments = match direct_arguments(arguments) {
        Ok(arguments) => arguments,
        Err(message) => return fail(message),
    };
    let outcome = manager
        .call_with_progress(
            &server,
            &tool,
            Value::Object(arguments),
            cancellation,
            progress,
        )
        .await
        .map(|value| render_call_tool_result(&value, &server, &tool, env));
    mcp_outcome_result(name, outcome)
}

impl Runtime {
    /// Tool definitions advertised for a loop: the cached native set plus the
    /// MCP tools declared directly. Direct tools are Auto-only like the
    /// `mcp` gateway; the native set is returned untouched (same `Arc`) when
    /// there are none.
    pub(super) fn tool_definition_set(
        &self,
        mode: crate::OperatingMode,
        code_intel_enabled: bool,
    ) -> Arc<[Value]> {
        let base = self.base_tool_definition_set(mode, code_intel_enabled);
        if !mode.allows_mutation() {
            return base;
        }
        let Some(manager) = self.mcp.as_ref() else {
            return base;
        };
        let direct = manager.direct_tool_definitions();
        if direct.is_empty() {
            return base;
        }
        let mut all = base.to_vec();
        all.extend(direct);
        all.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_arguments_accept_objects_and_empty_input_only() {
        assert!(direct_arguments("").unwrap().is_empty());
        assert!(direct_arguments("  \n").unwrap().is_empty());
        assert!(direct_arguments("null").unwrap().is_empty());
        assert_eq!(
            Value::Object(direct_arguments(r#"{"a":1}"#).unwrap()),
            json!({"a": 1})
        );
        for bad in ["[]", "1", "\"x\"", "{", "true"] {
            assert!(direct_arguments(bad)
                .unwrap_err()
                .contains("invalid MCP tool arguments"));
        }
    }

    struct Quiet;

    #[async_trait::async_trait]
    impl crate::mcp::McpConnection for Quiet {
        async fn request(
            &self,
            _method: &str,
            _params: Value,
        ) -> Result<Value, crate::mcp::McpError> {
            Ok(json!({"content": [{"type": "text", "text": "ok"}]}))
        }

        async fn notify(&self, _method: &str, _params: Value) {}

        fn is_closed(&self) -> bool {
            false
        }
    }

    fn manager_with_direct_tool() -> Arc<McpManager> {
        let manager = Arc::new(McpManager::new(
            Default::default(),
            std::path::PathBuf::from("."),
            Default::default(),
        ));
        let mut spec = crate::mcp::McpServerSpec::new(
            "fs",
            crate::mcp::McpTransport::Stdio {
                command: "controlled-test-connection".into(),
                args: Vec::new(),
                env: Default::default(),
            },
        );
        spec.options.exposure = crate::mcp::McpExposure::Direct;
        manager.insert_connection(
            spec,
            Arc::new(Quiet),
            vec![crate::mcp::McpToolSummary {
                name: "read".into(),
                description: Some("Reads".into()),
                schema: json!({"type": "object"}),
                output_schema: None,
            }],
        );
        manager
    }

    #[test]
    fn definition_set_adds_direct_tools_in_auto_only_and_keeps_the_native_set_otherwise() {
        let mut runtime = Runtime::new();
        let native = runtime.advertised_tool_definitions(crate::OperatingMode::Auto);
        assert!(native
            .iter()
            .all(|tool| !tool["name"].as_str().unwrap().starts_with("mcp__")));
        runtime.set_mcp_manager(Some(manager_with_direct_tool()));
        let auto = runtime.advertised_tool_definitions(crate::OperatingMode::Auto);
        assert_eq!(
            auto.len(),
            native.len() + 3,
            "mcp + codemode + the direct tool"
        );
        let direct = auto.last().unwrap();
        assert_eq!(direct["name"], "mcp__fs__read");
        assert_eq!(
            direct["input_schema"],
            json!({"type": "object", "properties": {}})
        );
        // The gateway definition is the same constant text with or without direct tools.
        let gateway = auto.iter().find(|tool| tool["name"] == "mcp").unwrap();
        assert_eq!(gateway, &mcp_tool_definition());
        for mode in [crate::OperatingMode::ReadOnly, crate::OperatingMode::Plan] {
            let advertised = runtime.advertised_tool_definitions(mode);
            assert!(
                advertised
                    .iter()
                    .all(|tool| !tool["name"].as_str().unwrap().starts_with("mcp")),
                "{mode:?}"
            );
        }
        // Stable between requests while nothing changes.
        assert_eq!(
            auto,
            runtime.advertised_tool_definitions(crate::OperatingMode::Auto)
        );
    }

    #[test]
    fn direct_calls_are_serial_barriers_like_the_gateway() {
        let runtime = Runtime::new();
        let cwd = std::env::temp_dir();
        for name in ["mcp", "mcp__fs__read", "codemode"] {
            let prepared =
                runtime
                    .tools
                    .prepare_invocation(crate::OperatingMode::Auto, &cwd, name, "{}");
            assert!(super::super::is_serial_barrier(&prepared), "{name}");
        }
        let prepared = runtime.tools.prepare_invocation(
            crate::OperatingMode::Auto,
            &cwd,
            "read",
            r#"{"path":"."}"#,
        );
        assert!(!super::super::is_serial_barrier(&prepared));
    }

    #[tokio::test]
    async fn dispatch_routes_by_name_and_refuses_unknown_direct_names() {
        let manager = manager_with_direct_tool();
        let _ = manager.direct_tool_definitions();
        let env = RenderEnv::default();
        let (ok, _) = run_mcp_route(
            Some(manager.clone()),
            "mcp__fs__read",
            "",
            McpCancellation::new(),
            None,
            &env,
        )
        .await;
        assert!(ok.success, "{}", ok.output);
        assert_eq!(ok.name, "mcp__fs__read");
        assert_eq!(ok.output, "ok");
        let (unknown, _) = run_mcp_route(
            Some(manager.clone()),
            "mcp__fs__other",
            "{}",
            McpCancellation::new(),
            None,
            &env,
        )
        .await;
        assert!(!unknown.success);
        assert!(
            unknown.output.contains("unknown MCP tool"),
            "{}",
            unknown.output
        );
        let (bad, _) = run_mcp_route(
            Some(manager.clone()),
            "mcp__fs__read",
            "[1]",
            McpCancellation::new(),
            None,
            &env,
        )
        .await;
        assert!(
            !bad.success && bad.output.contains("expected a JSON object"),
            "{}",
            bad.output
        );
        let (none, _) = run_mcp_route(
            None,
            "mcp__fs__read",
            "{}",
            McpCancellation::new(),
            None,
            &env,
        )
        .await;
        assert!(!none.success && none.output.contains("mcp unavailable"));
        // The gateway keeps its own name and arguments contract.
        let (gateway, _) = run_mcp_route(
            Some(manager),
            "mcp",
            r#"{"server":"fs","tool":"read","arguments":{}}"#,
            McpCancellation::new(),
            None,
            &env,
        )
        .await;
        assert_eq!(gateway.name, "mcp");
        assert!(gateway.success, "{}", gateway.output);
    }

    #[test]
    fn the_awareness_block_follows_the_stanza_in_auto_only_and_is_stripped_with_it() {
        use crate::provider::ProviderMessage;
        let block = manager_with_direct_tool().awareness_block().unwrap();
        for (mode, expect) in [
            (crate::OperatingMode::Auto, true),
            (crate::OperatingMode::ReadOnly, false),
            (crate::OperatingMode::Plan, false),
        ] {
            let mut messages = vec![ProviderMessage::user("Continue")];
            let seen = {
                let frame = super::super::mode::ChannelFrame::new(&messages, Some(block.clone()));
                let overlay = super::super::mode::ChannelOverlay::apply_with_mcp(
                    &mut messages,
                    mode,
                    false,
                    None,
                    &frame,
                );
                overlay.view()[0].content.clone()
            };
            assert_eq!(seen.contains("MCP servers ("), expect, "{mode:?}: {seen}");
            if expect {
                let stanza = seen.find(super::super::mode::CHANNEL_MARKER).unwrap();
                assert!(stanza < seen.find("MCP servers (").unwrap());
                assert!(seen.contains("- fs (ready, 1 tool)"));
                assert_eq!(crate::without_workspace_snapshot(&seen), "Continue");
            }
            // The overlay is a view only.
            assert_eq!(messages[0].content, "Continue");
        }
        // A stale stanza (with an old block) is replaced, never duplicated.
        let mut messages = vec![ProviderMessage::user(format!(
            "Continue{}\n\nMCP servers (old)\n- gone (ready)",
            super::super::mode::channel_stanza(crate::OperatingMode::Auto, false)
        ))];
        let frame = super::super::mode::ChannelFrame::new(&messages, Some(block));
        let overlay = super::super::mode::ChannelOverlay::apply_with_mcp(
            &mut messages,
            crate::OperatingMode::Auto,
            false,
            None,
            &frame,
        );
        let seen = overlay.view()[0].content.clone();
        assert_eq!(seen.matches("MCP servers (").count(), 1, "{seen}");
        assert!(!seen.contains("gone"));
    }

    #[test]
    fn outcome_text_keeps_the_no_replay_wording_for_both_entry_points() {
        use crate::mcp::{McpCleanupStatus, McpInterruption};
        let (uncertain, _) = mcp_outcome_result(
            "mcp__fs__write",
            McpRequestOutcome::OutcomeUncertain {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::Unconfirmed,
            },
        );
        assert_eq!(uncertain.name, "mcp__fs__write");
        assert!(!uncertain.success);
        assert!(uncertain.output.contains("outcome is uncertain"));
        assert!(uncertain.output.contains("do not replay"));
        let (before, _) = mcp_outcome_result(
            "mcp",
            McpRequestOutcome::InterruptedBeforeSend {
                interruption: McpInterruption::Cancelled,
                cleanup: McpCleanupStatus::NotRequired,
            },
        );
        assert!(before.output.contains("stopped before tools/call was sent"));
        let (ok, blobs) = mcp_outcome_result(
            "mcp__a__b",
            McpRequestOutcome::Completed(Ok(Rendered::plain("out"))),
        );
        assert!(ok.success && ok.output == "out" && ok.media.is_empty() && blobs.is_empty());
        let (failed, _) = mcp_outcome_result(
            "mcp__a__b",
            McpRequestOutcome::Completed(Err(crate::mcp::McpError::Closed)),
        );
        assert!(!failed.success && failed.output.starts_with("mcp error:"));
    }

    /// Answers `tools/call` with an image, a text part carrying a secret and
    /// a binary resource, like a screenshot-and-dump tool.
    struct Rich;

    #[async_trait::async_trait]
    impl crate::mcp::McpConnection for Rich {
        async fn request(
            &self,
            _method: &str,
            _params: Value,
        ) -> Result<Value, crate::mcp::McpError> {
            Ok(json!({"content": [
                {"type": "text", "text": "saw tok-direct-secret"},
                {"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAI"},
                {"type": "resource", "resource": {
                    "uri": "file:///dump.bin", "mimeType": "application/octet-stream",
                    "blob": "AAEC/w=="}},
            ]}))
        }

        async fn notify(&self, _method: &str, _params: Value) {}

        fn is_closed(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn direct_tools_render_media_and_artifacts_like_the_gateway() {
        let root = std::env::temp_dir().join(format!(
            "slim-mcp-direct-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = crate::context::ArtifactStore::new(&root).unwrap();
        let manager = Arc::new(McpManager::new(
            Default::default(),
            std::env::temp_dir(),
            Default::default(),
        ));
        let mut spec = crate::mcp::McpServerSpec::new(
            "shots",
            crate::mcp::McpTransport::Stdio {
                command: "controlled-test-connection".into(),
                args: Vec::new(),
                env: Default::default(),
            },
        );
        spec.options.exposure = crate::mcp::McpExposure::Direct;
        manager.insert_connection(
            spec,
            Arc::new(Rich),
            vec![crate::mcp::McpToolSummary {
                name: "capture".into(),
                description: None,
                schema: json!({"type": "object"}),
                output_schema: None,
            }],
        );
        let _ = manager.direct_tool_definitions();
        let secrets = vec!["tok-direct-secret".to_owned()];
        let env = RenderEnv {
            store: Some(&store),
            secrets: &secrets,
        };
        let (direct, direct_blobs) = run_mcp_route(
            Some(manager.clone()),
            "mcp__shots__capture",
            "{}",
            McpCancellation::new(),
            None,
            &env,
        )
        .await;
        let (gateway, gateway_blobs) = run_mcp_route(
            Some(manager),
            "mcp",
            r#"{"server":"shots","tool":"capture","arguments":{}}"#,
            McpCancellation::new(),
            None,
            &env,
        )
        .await;
        assert_eq!(direct.name, "mcp__shots__capture");
        assert_eq!(gateway.name, "mcp");
        assert!(direct.success && gateway.success, "{}", direct.output);
        // The image reaches the model as a content block, not as text.
        assert_eq!(direct.media.len(), 1);
        assert_eq!(direct.media, gateway.media);
        assert!(
            direct.output.contains("[image image/png, "),
            "{}",
            direct.output
        );
        // The binary part is an artifact the output points at.
        assert_eq!(direct_blobs.len(), 1);
        assert_eq!(gateway_blobs.len(), 1);
        assert!(
            direct.output.contains(&direct_blobs[0].id),
            "{}",
            direct.output
        );
        assert_eq!(store.read(&direct_blobs[0]).unwrap(), vec![0, 1, 2, 255]);
        // Both entry points produce the same text apart from the artifact id.
        assert_eq!(
            direct.output.replace(&direct_blobs[0].id, "ID"),
            gateway.output.replace(&gateway_blobs[0].id, "ID")
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
