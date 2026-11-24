use std::path::PathBuf;

use super::*;
use crate::session::{DurableEntryRole, DurableRecord};

const DEFAULT_RECALL_LIMIT: usize = 6;
const MAX_RECALL_LIMIT: usize = 20;
const MAX_RECALL_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_SNIPPET_CHARS: usize = 1200;

pub(super) fn history_recall_definition() -> Value {
    serde_json::json!({
        "name": "history_recall",
        "description": "Search or read earlier pre-compaction messages from this session's durable archive without bloating the active prompt prefix.",
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Case-insensitive keyword or substring to search across earlier messages (space-separated words are matched with AND)."
                },
                "from_entry": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-based starting message index in the durable session archive."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "default": 6
                }
            },
            "additionalProperties": false
        }
    })
}

impl Runtime {
    pub fn set_session_history_archive(&mut self, path: Option<PathBuf>) {
        self.session_history_archive = path;
    }

    pub(super) fn execute_history_recall(
        &mut self,
        invocation: ToolInvocation<'_>,
        mut seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        let started_at = self.begin_tool(invocation, &mut seq)?;
        let mut result = match self.run_history_recall(invocation.arguments) {
            Ok(output) => ToolResult::ok(invocation.name, output),
            Err(error) => ToolResult::fail(invocation.name, error),
        };
        self.finish_tool(invocation, &mut result, started_at, &mut seq, None)?;
        Ok((result, seq))
    }

    fn run_history_recall(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            #[serde(default)]
            query: Option<String>,
            #[serde(default)]
            from_entry: Option<usize>,
            #[serde(default = "default_recall_limit")]
            limit: usize,
        }
        fn default_recall_limit() -> usize {
            DEFAULT_RECALL_LIMIT
        }

        let args: Arguments = serde_json::from_str(arguments)
            .map_err(|_| "history_recall expects optional query, from_entry, and limit".to_owned())?;
        let limit = args.limit.clamp(1, MAX_RECALL_LIMIT);
        let archive_path = self
            .session_history_archive
            .as_ref()
            .ok_or_else(|| "history_recall archive is unavailable in this session".to_owned())?;
        let preflight = crate::session::preflight_session(archive_path)
            .map_err(|error| format!("history_recall could not read session archive: {error}"))?;
        let entries: Vec<(usize, &DurableEntryRole, &str)> = preflight
            .records
            .iter()
            .filter_map(|record| match record {
                DurableRecord::Entry { entry, .. } => Some((&entry.role, entry.content.as_str())),
                _ => None,
            })
            .enumerate()
            .map(|(idx, (role, content))| (idx, role, content))
            .collect();
        let total = entries.len();
        let start = args.from_entry.unwrap_or(0).min(total);
        let terms: Vec<String> = args
            .query
            .as_deref()
            .map(|q| {
                q.split_whitespace()
                    .filter(|term| !term.is_empty())
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_default();
        let mut matches = Vec::new();
        let mut total_bytes = 0usize;
        for (idx, role, content) in entries.into_iter().skip(start) {
            if !terms.is_empty() {
                let lowered = content.to_lowercase();
                if !terms.iter().all(|term| lowered.contains(term.as_str())) {
                    continue;
                }
            }
            let snippet = truncate_snippet(content, MAX_SNIPPET_CHARS);
            let role_label = match role {
                DurableEntryRole::User => "user",
                DurableEntryRole::Assistant => "assistant",
                DurableEntryRole::Tool => "tool",
            };
            let entry_json = serde_json::json!({
                "entry": idx,
                "role": role_label,
                "content": snippet,
            });
            let est = entry_json.to_string().len();
            if !matches.is_empty() && total_bytes.saturating_add(est) > MAX_RECALL_OUTPUT_BYTES {
                break;
            }
            total_bytes = total_bytes.saturating_add(est);
            matches.push(entry_json);
            if matches.len() >= limit {
                break;
            }
        }
        let payload = serde_json::json!({
            "total_entries": total,
            "returned": matches.len(),
            "entries": matches,
        });
        serde_json::to_string(&payload).map_err(|error| error.to_string())
    }
}

fn truncate_snippet(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}… [truncated]")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{DurableEntry, DurableRepo, DurableSessionHeader, JsonlRepo};

    #[test]
    fn history_recall_searches_and_pages_durable_session_entries() {
        let dir = crate::runtime::temp_root::TempRoot::new("history-recall");
        let path = dir.join("session.jsonl");
        let header = DurableSessionHeader::new(
            "sess-recall",
            dir.to_string_lossy().into_owned(),
            "2026-11-20T00:00:00Z",
            None,
            None,
        );
        let mut repo = JsonlRepo::create(&path, header).unwrap();
        repo.append(DurableRecord::Entry {
            seq: 1,
            entry: DurableEntry::from_provider_message(
                "e1".into(),
                None,
                "op1".into(),
                ProviderMessage::user("We decided to use PostgreSQL with pgvector."),
            )
            .unwrap(),
        })
        .unwrap();
        repo.append(DurableRecord::Entry {
            seq: 2,
            entry: DurableEntry::from_provider_message(
                "e2".into(),
                Some("e1".into()),
                "op1".into(),
                ProviderMessage::assistant("Noted: PostgreSQL + pgvector chosen.", Vec::new()),
            )
            .unwrap(),
        })
        .unwrap();
        repo.append(DurableRecord::Entry {
            seq: 3,
            entry: DurableEntry::from_provider_message(
                "e3".into(),
                Some("e2".into()),
                "op2".into(),
                ProviderMessage::user("Now let's work on the CSS grid layout."),
            )
            .unwrap(),
        })
        .unwrap();

        let mut runtime = Runtime::new();
        runtime.set_session_history_archive(Some(path));
        let (result, _) = runtime
            .execute_tool(
                crate::OperatingMode::Auto,
                &dir,
                "history_recall",
                r#"{"query":"postgresql pgvector","limit":5}"#,
                1,
            )
            .unwrap();
        assert!(result.success, "{}", result.output);
        let parsed: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(parsed["total_entries"], 3);
        assert_eq!(parsed["returned"], 2);
        assert_eq!(parsed["entries"][0]["entry"], 0);
        assert_eq!(parsed["entries"][1]["entry"], 1);
    }
}
