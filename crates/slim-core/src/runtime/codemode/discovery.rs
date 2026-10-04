//! Discovery helpers of codemode (`searchTools`, `describeTool`,
//! `listServers`). They read the same catalogs, ranking and hidden-tool
//! filter as the `mcp` gateway and never consume the tool budget. Like every
//! host call they run one at a time: the interpreter bridge is synchronous,
//! so parallel host calls are not supported.

use super::*;
use crate::mcp::{McpCleanupStatus, McpInterruption};
use engine::SearchArgs;

/// `searchTools()` hits when the script gives no limit (Pi's default).
const DEFAULT_SEARCH_LIMIT: usize = 8;

impl Runtime {
    pub(super) async fn codemode_discovery(
        &mut self,
        command: Command,
        cell: &engine::Cell,
        run: &CancellationToken,
    ) -> Result<Value, String> {
        if run.is_cancelled() || cell.cancellation.is_cancelled() || Instant::now() >= cell.deadline
        {
            return Err("cell cancelled or deadline exceeded".into());
        }
        let Some(manager) = self.mcp.clone() else {
            return Err("no MCP servers configured".into());
        };
        let cancellation = cell.cancellation.clone();
        let pending = async {
            match command {
                Command::SearchTools(SearchArgs {
                    query,
                    limit,
                    server,
                }) => {
                    let limit = limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
                    if limit == 0 {
                        return McpRequestOutcome::Completed(Err(crate::mcp::McpError::Protocol(
                            "searchTools() limit must be a positive integer".into(),
                        )));
                    }
                    manager
                        .search_tools_value(server.as_deref(), &query, limit, cancellation)
                        .await
                }
                Command::DescribeTool { name } => {
                    manager.describe_tool_value(&name, cancellation).await
                }
                _ => McpRequestOutcome::Completed(Ok(manager.list_servers_value())),
            }
        };
        tokio::pin!(pending);
        let outcome = tokio::select! {
            biased;
            _ = run.cancelled() => { cell.cancellation.cancel(); pending.await },
            _ = tokio::time::sleep_until(cell.deadline.into()) => { cell.cancellation.cancel(); pending.await },
            outcome = &mut pending => outcome,
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
        redact_value(&mut value, &self.sensitive_values.0);
        if value.to_string().len() > MAX_JSON_BYTES {
            return Err("discovery result exceeds the 1 MiB bridge limit; narrow the query".into());
        }
        Ok(value)
    }
}

fn interrupted(interruption: McpInterruption, cleanup: McpCleanupStatus) -> String {
    format!("MCP discovery interrupted: {interruption:?}; cleanup: {cleanup:?}")
}
