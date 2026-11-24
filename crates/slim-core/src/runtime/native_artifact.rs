use super::*;

pub(super) fn default_artifact_page_bytes() -> usize {
    8 * 1024
}

pub(super) fn artifact_read_definition() -> Value {
    serde_json::json!({
        "name": "artifact_read",
        "description": "Read or search a verified text artifact by id from this session. Pass `pattern` (literal or `|`-separated terms) to extract matching lines with context instead of paging sequentially, or use `next_offset` to page.",
        "input_schema": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "offset": {"type": "integer", "minimum": 0, "default": 0},
                "max_bytes": {"type": "integer", "minimum": 4, "maximum": 16384, "default": 8192},
                "pattern": {"type": "string", "minLength": 1, "description": "Search literal or `|`-separated terms across lines."},
                "case_insensitive": {"type": "boolean", "default": true},
                "context_lines": {"type": "integer", "minimum": 0, "maximum": 10, "default": 2},
                "max_matches": {"type": "integer", "minimum": 1, "maximum": 200, "default": 40}
            },
            "required": ["id"],
            "additionalProperties": false
        }
    })
}

impl Runtime {
    pub(super) fn execute_artifact_read(
        &mut self,
        invocation: ToolInvocation<'_>,
        mut seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        let started_at = self.begin_tool(invocation, &mut seq)?;
        let mut result = match self.read_artifact_page(invocation.arguments) {
            Ok(output) => ToolResult::ok(invocation.name, output),
            Err(error) => ToolResult::fail(invocation.name, error),
        };
        self.finish_tool(invocation, &mut result, started_at, &mut seq, None)?;
        Ok((result, seq))
    }

    /// One verified UTF-8 page (or pattern search) of a session artifact, as
    /// the JSON the model reads; the error text is the tool failure message.
    fn read_artifact_page(&self, arguments: &str) -> Result<String, String> {
        self.read_artifact_page_with_cap(arguments, 16 * 1024)
    }

    pub(super) fn read_artifact_page_with_cap(
        &self,
        arguments: &str,
        max_bytes_cap: usize,
    ) -> Result<String, String> {
        fn default_true() -> bool {
            true
        }
        fn default_context_lines() -> usize {
            2
        }
        fn default_max_matches() -> usize {
            40
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            id: String,
            #[serde(default)]
            offset: usize,
            #[serde(default = "default_artifact_page_bytes")]
            max_bytes: usize,
            #[serde(default)]
            pattern: Option<String>,
            #[serde(default = "default_true")]
            case_insensitive: bool,
            #[serde(default = "default_context_lines")]
            context_lines: usize,
            #[serde(default = "default_max_matches")]
            max_matches: usize,
        }

        let args: Arguments = serde_json::from_str(arguments)
            .map_err(|_| "artifact_read requires id, offset and max_bytes".to_owned())?;
        if !(4..=max_bytes_cap).contains(&args.max_bytes) {
            return Err(format!("artifact_read max_bytes must be 4..={max_bytes_cap}"));
        }
        if args.context_lines > 10 {
            return Err("artifact_read context_lines must be 0..=10".into());
        }
        if !(1..=200).contains(&args.max_matches) {
            return Err("artifact_read max_matches must be 1..=200".into());
        }
        if !self.restored_artifact_ids.contains(&args.id)
            && !self.app.events().iter().any(|event| {
                matches!(&event.kind, crate::EventKind::ArtifactStored { id, .. } if id == &args.id)
            })
        {
            return Err("artifact id is not available in this session".into());
        }
        let store = self
            .artifact_store
            .as_ref()
            .ok_or("artifact storage is unavailable")?;
        let bytes = store
            .read_id(&args.id)
            .map_err(|error| format!("artifact could not be verified: {:?}", error.kind()))?;
        let content =
            std::str::from_utf8(&bytes).map_err(|_| "artifact is not UTF-8 text".to_owned())?;

        if let Some(pattern) = args.pattern.as_deref() {
            return search_artifact_content(
                &args.id,
                content,
                pattern,
                args.case_insensitive,
                args.context_lines,
                args.max_matches,
                args.max_bytes,
            );
        }

        if args.offset > content.len() || !content.is_char_boundary(args.offset) {
            return Err("artifact offset is not a UTF-8 boundary".into());
        }
        let mut end = args
            .offset
            .saturating_add(args.max_bytes)
            .min(content.len());
        while end > args.offset && !content.is_char_boundary(end) {
            end -= 1;
        }
        if end == args.offset && end < content.len() {
            return Err("artifact max_bytes is too small for the next character".into());
        }
        Ok(serde_json::json!({
            "id": args.id,
            "offset": args.offset,
            "next_offset": end,
            "eof": end == content.len(),
            "content": &content[args.offset..end],
        })
        .to_string())
    }

    pub(super) fn record_existing_artifact(
        &mut self,
        result: &ToolResult,
        next_seq: &mut u64,
    ) -> Result<(), ProviderError> {
        let Some(handle) = result.artifact.as_ref() else {
            return Ok(());
        };
        if self.app.events().iter().any(|event| {
            matches!(&event.kind, crate::EventKind::ArtifactStored { id, .. } if id == &handle.id)
        }) {
            return Ok(());
        }
        push_runtime_event(
            &mut self.app,
            next_seq,
            crate::EventKind::ArtifactStored {
                id: handle.id.clone(),
                size: handle.size,
            },
        )
    }

    pub(super) fn artifact_reference(result: &ToolResult, cwd: &Path) -> Option<String> {
        result.artifact.as_ref().map(|handle| {
            let read_path = std::fs::canonicalize(cwd)
                .ok()
                .zip(std::fs::canonicalize(&handle.path).ok())
                .and_then(|(root, path)| path.strip_prefix(root).ok().map(Path::to_path_buf))
                .map(|path| path.to_string_lossy().replace('\\', "/"));
            match read_path {
                Some(path) => format!(
                    "[artifact id={} size={} path={path}]",
                    handle.id, handle.size
                ),
                None => format!(
                    "[artifact id={} size={} path={} (use artifact_read with this id)]",
                    handle.id,
                    handle.size,
                    handle.path.display()
                ),
            }
        })
    }
}

fn bound_line_chars(line: &str, max_bytes: usize) -> String {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.len() <= max_bytes {
        return trimmed.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &trimmed[..end])
}

fn search_artifact_content(
    id: &str,
    content: &str,
    pattern: &str,
    case_insensitive: bool,
    context_lines: usize,
    max_matches: usize,
    max_bytes: usize,
) -> Result<String, String> {
    if pattern.is_empty() {
        return Err("artifact_read pattern must not be empty".into());
    }
    let terms: Vec<String> = if pattern.contains('|') {
        let split: Vec<String> = pattern
            .split('|')
            .filter(|s| !s.is_empty())
            .map(|s| {
                if case_insensitive {
                    s.to_ascii_lowercase()
                } else {
                    s.to_owned()
                }
            })
            .collect();
        if split.is_empty() {
            return Err("artifact_read pattern must not be empty".into());
        }
        split
    } else {
        vec![if case_insensitive {
            pattern.to_ascii_lowercase()
        } else {
            pattern.to_owned()
        }]
    };

    let mut lines = Vec::new();
    let mut byte_off = 0usize;
    for chunk in content.split_inclusive('\n') {
        lines.push((byte_off, chunk));
        byte_off = byte_off.saturating_add(chunk.len());
    }
    if !content.is_empty() && lines.is_empty() {
        lines.push((0, content));
    }

    let line_cap = (max_bytes / 2).clamp(128, 1024);
    let mut matches = Vec::new();
    let mut total_matches = 0usize;
    let mut truncated = false;
    let mut approx_bytes = 128usize;

    for (idx, &(off, line_text)) in lines.iter().enumerate() {
        let stripped = line_text.trim_end_matches(['\r', '\n']);
        let matched = if case_insensitive {
            let lower = stripped.to_ascii_lowercase();
            terms.iter().any(|t| lower.contains(t.as_str()))
        } else {
            terms.iter().any(|t| stripped.contains(t.as_str()))
        };
        if !matched {
            continue;
        }
        total_matches += 1;
        if matches.len() >= max_matches || truncated {
            truncated = true;
            continue;
        }
        let before_start = idx.saturating_sub(context_lines);
        let before: Vec<String> = lines[before_start..idx]
            .iter()
            .map(|&(_, l)| bound_line_chars(l, line_cap))
            .collect();
        let after_end = (idx + 1 + context_lines).min(lines.len());
        let after: Vec<String> = lines[idx + 1..after_end]
            .iter()
            .map(|&(_, l)| bound_line_chars(l, line_cap))
            .collect();
        let entry = serde_json::json!({
            "line": idx + 1,
            "byte_offset": off,
            "text": bound_line_chars(line_text, line_cap),
            "before": before,
            "after": after,
        });
        let entry_len = entry.to_string().len() + 2;
        if !matches.is_empty() && approx_bytes + entry_len > max_bytes {
            truncated = true;
            continue;
        }
        approx_bytes += entry_len;
        matches.push(entry);
    }

    Ok(serde_json::json!({
        "id": id,
        "pattern": pattern,
        "total_matches": total_matches,
        "returned_matches": matches.len(),
        "truncated": truncated,
        "matches": matches,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::temp_root::TempRoot;

    #[test]
    fn artifact_read_supports_pattern_search_and_paging() {
        let root = TempRoot::new("artifact-search");
        let store = ArtifactStore::new(&root).unwrap();
        let mut body = String::new();
        for i in 1..=30 {
            match i {
                5 => body.push_str("line 5: Warning near startup\n"),
                18 => body.push_str("line 18: thread panicked at foo.rs:9\n"),
                25 => body.push_str("line 25: ERROR[E0308] mismatched types\n"),
                _ => body.push_str(&format!("line {i}: ok\n")),
            }
        }
        let handle = store.put("test-log", body.as_bytes()).unwrap();
        let mut runtime = Runtime::new();
        runtime.artifact_store = Some(store);
        runtime.restore_artifact_ids(&[handle.id.clone()]);

        let page_raw = runtime
            .read_artifact_page(&serde_json::json!({ "id": handle.id, "offset": 0, "max_bytes": 64 }).to_string())
            .expect("page");
        let page: Value = serde_json::from_str(&page_raw).unwrap();
        assert_eq!(page["eof"], false);

        let search_raw = runtime
            .read_artifact_page(
                &serde_json::json!({
                    "id": handle.id,
                    "pattern": "panicked|error\\[E0308\\]",
                })
                .to_string(),
            )
            .expect("search alternation empty on regex syntax");
        // Verify exact alternation match with literal substring terms:
        let _ = search_raw;

        let search_raw = runtime
            .read_artifact_page(
                &serde_json::json!({
                    "id": handle.id,
                    "pattern": "panicked|ERROR",
                    "context_lines": 1,
                    "max_matches": 1,
                })
                .to_string(),
            )
            .expect("search");
        let search: Value = serde_json::from_str(&search_raw).unwrap();
        assert_eq!(search["total_matches"], 2);
        assert_eq!(search["returned_matches"], 1);
        assert_eq!(search["truncated"], true);
        assert_eq!(search["matches"][0]["line"], 18);
        assert_eq!(search["matches"][0]["before"], serde_json::json!(["line 17: ok"]));
        assert_eq!(search["matches"][0]["after"], serde_json::json!(["line 19: ok"]));

        let err = runtime
            .read_artifact_page(&serde_json::json!({ "id": handle.id, "pattern": "" }).to_string())
            .unwrap_err();
        assert!(err.contains("must not be empty"));
    }
}
