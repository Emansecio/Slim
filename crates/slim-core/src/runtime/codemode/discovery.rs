//! Discovery helpers of codemode (`searchTools`, `describeTool`,
//! `listServers`). They read the same catalogs, ranking and hidden-tool
//! filter as the `mcp` gateway and never consume the tool budget. They are
//! synchronous for the program: the worker blocks until the host replies,
//! while calls already in flight keep running. In Code mode `describeTool`
//! also describes `slim.<tool>` and adds a TypeScript `signature`.

use super::*;
use crate::mcp::{McpCleanupStatus, McpInterruption};
use engine::SearchArgs;

/// `searchTools()` hits when the script gives no limit (Pi's default).
const DEFAULT_SEARCH_LIMIT: usize = 8;

impl Runtime {
    pub(super) async fn codemode_discovery(
        &mut self,
        command: Command,
        scope: &CellScope<'_>,
        cell: &engine::Cell,
    ) -> Result<Value, String> {
        if cell.cancellation.is_cancelled() {
            return Err("cell cancelled or deadline exceeded".into());
        }
        if let Command::DescribeTool { name } = &command {
            if let Some(native) = name
                .strip_prefix("slim.")
                .filter(|_| !scope.natives.is_empty())
            {
                return Ok(scope.native(native).map_or(Value::Null, |definition| {
                    describe_native(name, native, definition)
                }));
            }
        }
        let Some(manager) = self.mcp.clone() else {
            return Err("no MCP servers configured".into());
        };
        // The cell's token is cancelled by the run and by the deadline.
        let cancellation = cell.cancellation.mcp_cancellation();
        let outcome = match command {
            Command::SearchTools(SearchArgs {
                query,
                limit,
                server,
            }) => {
                let limit = limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
                if limit == 0 {
                    return Err("searchTools() limit must be a positive integer".into());
                }
                manager
                    .search_tools_value(server.as_deref(), &query, limit, cancellation)
                    .await
            }
            Command::DescribeTool { name } => {
                manager.describe_tool_value(&name, cancellation).await
            }
            _ => McpRequestOutcome::Completed(Ok(manager.list_servers_value())),
        };
        let mut value = match outcome {
            McpRequestOutcome::Completed(Ok(value)) => value,
            McpRequestOutcome::Completed(Err(error)) => return Err(error.to_string()),
            McpRequestOutcome::InterruptedBeforeSend {
                interruption,
                cleanup,
            }
            | McpRequestOutcome::OutcomeUncertain {
                interruption,
                cleanup,
            } => return Err(interrupted(interruption, cleanup)),
        };
        if !scope.natives.is_empty() {
            add_signature(&mut value);
        }
        redact_value(&mut value, &self.sensitive_values.0);
        if value.to_string().len() > MAX_JSON_BYTES {
            return Err("discovery result exceeds the 1 MiB bridge limit; narrow the query".into());
        }
        Ok(value)
    }
}

/// `describeTool('slim.<tool>')`: the native definition in the shape of an
/// MCP description, with the signature of `slim.<tool>` as declared. Natives
/// resolve to their text output.
fn describe_native(name: &str, native: &str, definition: &Value) -> Value {
    let description = definition["description"].as_str();
    let schema = &definition["input_schema"];
    let returns = definition
        .get("return_type")
        .and_then(Value::as_str)
        .unwrap_or("string");
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
        "signature": typescript::signature(native, description, schema, returns),
    })
}

/// Adds the TypeScript signature to a described MCP tool. The call resolves
/// to `structuredContent`, typed by `outputSchema`, when the tool has one.
fn add_signature(described: &mut Value) {
    let Some(name) = described["name"].as_str() else {
        return;
    };
    let returns = described
        .get("outputSchema")
        .map_or_else(|| "unknown".to_owned(), typescript::ts_type);
    described["signature"] = Value::String(typescript::signature(
        name,
        described["description"].as_str(),
        &described["inputSchema"],
        &returns,
    ));
}

fn interrupted(interruption: McpInterruption, cleanup: McpCleanupStatus) -> String {
    format!("MCP discovery interrupted: {interruption:?}; cleanup: {cleanup:?}")
}
