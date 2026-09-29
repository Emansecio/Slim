use super::*;

pub(super) fn default_artifact_page_bytes() -> usize {
    8 * 1024
}

pub(super) fn artifact_read_definition() -> Value {
    serde_json::json!({
        "name": "artifact_read",
        "description": "Read a verified text artifact by id from this session. Use next_offset to continue; an incomplete shell capture is marked in the artifact.",
        "input_schema": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "offset": {"type": "integer", "minimum": 0, "default": 0},
                "max_bytes": {"type": "integer", "minimum": 4, "maximum": 16384, "default": 8192}
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

    /// One verified UTF-8 page of a session artifact, as the JSON the model
    /// reads; the error text is the tool failure message.
    fn read_artifact_page(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            id: String,
            #[serde(default)]
            offset: usize,
            #[serde(default = "default_artifact_page_bytes")]
            max_bytes: usize,
        }

        let args: Arguments = serde_json::from_str(arguments)
            .map_err(|_| "artifact_read requires id, offset and max_bytes".to_owned())?;
        if !(4..=16 * 1024).contains(&args.max_bytes) {
            return Err("artifact_read max_bytes must be 4..=16384".into());
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
