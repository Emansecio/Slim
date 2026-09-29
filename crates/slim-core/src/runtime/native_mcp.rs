use super::*;

pub(super) fn mcp_tool_definition() -> Value {
    json!({
        "name": "mcp",
        "description": "MCP bridge to configured external tool servers. {list:true} → servers+status (never connects). {server,list:true,offset} → its tools paged, offset defaults 0 (connects lazily). {server,tool,describe:true} → tool input schema. {server,tool,arguments:{...}} → call the tool.",
        "input_schema": {
            "type": "object",
            "properties": {
                "list": {"type": "boolean"},
                "server": {"type": "string"},
                "tool": {"type": "string"},
                "describe": {"type": "boolean"},
                "arguments": {"type": "object"},
                "offset": {"type": "integer", "minimum": 0}
            },
            "additionalProperties": false
        }
    })
}

/// Upper bound for one MCP call's rendered output; the provider-facing copy
/// is additionally capped by `max_result_bytes` downstream.
pub(super) const MAX_MCP_CALL_OUTPUT_BYTES: usize = 64 * 1024;

pub(super) async fn run_mcp_dispatch(
    manager: Option<Arc<McpManager>>,
    arguments: &str,
    cancellation: McpCancellation,
) -> ToolResult {
    fn result(output: impl Into<String>, success: bool) -> ToolResult {
        ToolResult::new("mcp", success, output)
    }
    let Some(manager) = manager else {
        return result(
            "mcp unavailable: no MCP servers configured (set [mcp.servers] in slim.toml)",
            false,
        );
    };
    let args: Value = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(error) => return result(format!("invalid mcp arguments: {error}"), false),
    };
    let usage = || {
        crate::mcp::McpError::Protocol(
            "usage: {list:true} | {server,list:true} | {server,tool,describe:true} | {server,tool,arguments}".into(),
        )
    };
    let list = match args.get("list") {
        None | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return result(format!("mcp error: {}", usage()), false),
    };
    let describe = match args.get("describe") {
        None | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return result(format!("mcp error: {}", usage()), false),
    };
    let offset = match args.get("offset") {
        None | Some(Value::Null) => 0usize,
        Some(value) => match value.as_u64() {
            Some(offset) => offset as usize,
            None => return result(format!("mcp error: {}", usage()), false),
        },
    };
    let server = args.get("server").and_then(Value::as_str);
    let tool = args.get("tool").and_then(Value::as_str);
    // Completed output paired with its success flag.
    let outcome: McpRequestOutcome<(String, bool)> = match (list, server, tool, describe) {
        (true, None, None, _) => McpRequestOutcome::Completed(Ok((manager.list_servers(), true))),
        (true, Some(server), _, _) => manager
            .list_tools_text_cancellable(server, offset, cancellation.clone())
            .await
            .map(|text| (text, true)),
        (false, Some(server), Some(tool), true) => manager
            .describe_cancellable(server, tool, cancellation.clone())
            .await
            .map(|text| (text, true)),
        (false, Some(server), Some(tool), false) => {
            let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
            manager
                .call_cancellable(server, tool, arguments, cancellation)
                .await
                .map(|value| render_mcp_call_result(&value))
        }
        _ => McpRequestOutcome::Completed(Err(usage())),
    };
    match outcome {
        McpRequestOutcome::Completed(Ok((output, success))) => result(output, success),
        McpRequestOutcome::Completed(Err(error)) => result(format!("mcp error: {error}"), false),
        McpRequestOutcome::InterruptedBeforeSend {
            interruption,
            cleanup,
        } => result(
            format!("mcp operation stopped before tools/call was sent: {interruption:?}; transport cleanup: {cleanup:?}"),
            false,
        ),
        McpRequestOutcome::OutcomeUncertain { interruption, cleanup } => result(
            format!(
                "mcp operation outcome is uncertain: {interruption:?}; transport cleanup: {cleanup:?}; do not replay the call automatically"
            ),
            false,
        ),
    }
}

/// Renders a `tools/call` result: text content concatenated, non-text items
/// serialized compactly, `isError` mapped to tool failure. Output is bounded
/// so a hostile server cannot flood the event log/journal. Returns the output
/// and whether the call succeeded.
pub(super) fn render_mcp_call_result(value: &Value) -> (String, bool) {
    let is_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let output = match value.get("content").and_then(Value::as_array) {
        Some(content) => {
            let mut text = String::new();
            for item in content {
                if !text.is_empty() {
                    text.push('\n');
                }
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        text.push_str(item.get("text").and_then(Value::as_str).unwrap_or(""))
                    }
                    _ => text.push_str(&serde_json::to_string(item).unwrap_or_default()),
                }
                if text.len() > MAX_MCP_CALL_OUTPUT_BYTES {
                    break;
                }
            }
            text
        }
        None => serde_json::to_string_pretty(value).unwrap_or_default(),
    };
    (
        truncate_result(&output, MAX_MCP_CALL_OUTPUT_BYTES),
        !is_error,
    )
}

impl Runtime {
    /// Runs one `mcp` meta-tool call against the shared manager. The manager
    /// connects lazily on first use; list/describe keep schemas out of the
    /// prompt until the model asks for them. Events use `invocation.name`
    /// (the `mcp` meta-tool), not a per-server tool name.
    pub(super) async fn execute_mcp(
        &mut self,
        mode: crate::OperatingMode,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut seq = next_seq;
        let started_at = self.begin_tool(invocation, &mut seq)?;
        let cancellation = self
            .cancellation
            .as_ref()
            .map(CancellationToken::mcp_cancellation)
            .unwrap_or_default();
        let mut result = if mode.allows_mutation() {
            run_mcp_dispatch(self.mcp.clone(), invocation.arguments, cancellation).await
        } else {
            ToolResult::fail("mcp", "mcp is only available in Auto mode")
        };
        self.finish_tool(invocation, &mut result, started_at, &mut seq, None)?;
        Ok((result, seq))
    }
}
