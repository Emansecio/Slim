use super::native_mcp_direct::{mcp_outcome_result, run_mcp_route};
use super::*;
use crate::context::ArtifactHandle;
use crate::mcp::{McpProgress, McpProgressSink};

pub(super) mod render;
mod resources;
use render::{limit_text, render_call_tool_result, RenderEnv, Rendered};

pub(super) fn mcp_tool_definition() -> Value {
    let mut definition = json!({
        "name": "mcp",
        "description": "Discover MCP tools: {list:true} lists servers without connecting; {server,list:true,offset} lists tools lazily; {query,server?,offset?} searches names/descriptions (without server, connected catalogs only); {server,tool,describe:true} returns schemas. {server,tool,arguments:{...}} calls one tool. Use codemode to compose calls and filter structured results before returning a summary.",
        "input_schema": {
            "type": "object",
            "properties": {
                "list": {"type": "boolean"},
                "server": {"type": "string"},
                "tool": {"type": "string"},
                "describe": {"type": "boolean"},
                "query": {"type": "string", "minLength": 1, "maxLength": 512},
                "arguments": {"type": "object"},
                "offset": {"type": "integer", "minimum": 0}
            },
            "additionalProperties": false
        }
    });
    resources::extend_tool_definition(&mut definition);
    definition
}

/// Progress updates a call may queue before the runtime drains them; excess
/// updates are dropped (a hostile server cannot grow memory or flood events).
const PROGRESS_QUEUE_CAPACITY: usize = 16;
/// At most one progress event per interval; the completing update always
/// passes.
const PROGRESS_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Bounded channel feeding progress of one MCP call to the runtime, which
/// owns the event stream.
fn progress_channel() -> (McpProgressSink, tokio::sync::mpsc::Receiver<McpProgress>) {
    let (sender, receiver) = tokio::sync::mpsc::channel(PROGRESS_QUEUE_CAPACITY);
    let last_sent = std::sync::Mutex::new(None::<Instant>);
    let sink: McpProgressSink = Arc::new(move |update: McpProgress| {
        let finishing = update
            .total
            .is_some_and(|total| total > 0.0 && update.progress >= total);
        let mut last = last_sent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !finishing && last.is_some_and(|sent| sent.elapsed() < PROGRESS_MIN_INTERVAL) {
            return;
        }
        if sender.try_send(update).is_ok() {
            *last = Some(Instant::now());
        }
    });
    (sink, receiver)
}

fn progress_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

/// One-line, bounded progress text for the tool progress event.
fn progress_preview(update: &McpProgress) -> String {
    let mut preview = match update.total {
        Some(total) if total > 0.0 => format!(
            "progress {}/{} ({:.0}%)",
            progress_number(update.progress),
            progress_number(total),
            (update.progress / total * 100.0).clamp(0.0, 100.0)
        ),
        _ => format!("progress {}", progress_number(update.progress)),
    };
    if let Some(message) = &update.message {
        preview.push_str(" - ");
        preview.push_str(message);
    }
    preview
}

/// Runs one `mcp` gateway call. The rendered text is not yet bounded: the
/// caller applies the inline budget ([`limit_text`]) after redaction, which
/// is also when the full text is stored. Returns the result with its media
/// attached and the artifacts created for binary parts.
pub(super) async fn run_mcp_dispatch_with(
    manager: Option<Arc<McpManager>>,
    arguments: &str,
    cancellation: McpCancellation,
    progress: Option<McpProgressSink>,
    env: &RenderEnv<'_>,
) -> (ToolResult, Vec<ArtifactHandle>) {
    fn result(output: impl Into<String>, success: bool) -> ToolResult {
        ToolResult::new("mcp", success, output)
    }
    let fail = |output: String| (result(output, false), Vec::new());
    let Some(manager) = manager else {
        return fail(
            "mcp unavailable: no MCP servers configured (set [mcp.servers] in slim.toml)".into(),
        );
    };
    #[derive(serde::Deserialize, Default)]
    #[serde(default, deny_unknown_fields)]
    struct Arguments {
        list: bool,
        describe: bool,
        server: Option<String>,
        tool: Option<String>,
        query: Option<String>,
        arguments: Option<serde_json::Map<String, Value>>,
        offset: Option<usize>,
        resources: bool,
        resource_templates: bool,
        uri: Option<String>,
        cursor: Option<String>,
    }
    let args: Arguments = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(error) => return fail(format!("invalid mcp arguments: {error}")),
    };
    let usage = || {
        crate::mcp::McpError::Protocol(
            "usage: {list:true} | {server,list:true} | {query,server?,offset?} | {server,tool,describe:true} | {server,tool,arguments} | {resources:true,server?,cursor?} | {resource_templates:true,server?,cursor?} | {server,uri}".into(),
        )
    };
    let list = args.list;
    let describe = args.describe;
    let offset = args.offset.unwrap_or_default();
    let server = args.server.as_deref();
    let tool = args.tool.as_deref();
    let resource_action = match resources::parse_action(&resources::ResourceFields {
        resources: args.resources,
        resource_templates: args.resource_templates,
        uri: args.uri.as_deref(),
        cursor: args.cursor.as_deref(),
        server,
        discovery_or_call: list
            || describe
            || tool.is_some()
            || args.query.is_some()
            || args.arguments.is_some(),
    }) {
        Ok(action) => action,
        Err(message) => return fail(format!("mcp error: {message}")),
    };
    let outcome: McpRequestOutcome<Rendered> = if let Some(action) = resource_action {
        resources::dispatch(&manager, action, cancellation, env).await
    } else if let Some(query) = args.query.as_deref() {
        if list || describe || tool.is_some() || args.arguments.is_some() {
            return fail(
                "mcp error: query cannot be combined with list, describe, tool or arguments".into(),
            );
        }
        manager
            .search_tools_cancellable(server, query, offset, cancellation)
            .await
            .map(Rendered::plain)
    } else {
        match (list, server, tool, describe) {
            (true, None, None, _) => {
                McpRequestOutcome::Completed(Ok(Rendered::plain(manager.list_servers())))
            }
            (true, Some(server), _, _) => manager
                .list_tools_text_cancellable(server, offset, cancellation.clone())
                .await
                .map(Rendered::plain),
            (false, Some(server), Some(tool), true) => manager
                .describe_cancellable(server, tool, cancellation.clone())
                .await
                .map(Rendered::plain),
            (false, Some(server), Some(tool), false) => {
                let arguments = Value::Object(args.arguments.unwrap_or_default());
                manager
                    .call_with_progress(server, tool, arguments, cancellation, progress)
                    .await
                    .map(|value| render_call_tool_result(&value, server, tool, env))
            }
            _ => McpRequestOutcome::Completed(Err(usage())),
        }
    };
    mcp_outcome_result("mcp", outcome)
}

/// [`run_mcp_dispatch_with`] without an artifact store, bounded like a
/// result the runtime presents.
#[cfg(test)]
pub(super) async fn run_mcp_dispatch(
    manager: Option<Arc<McpManager>>,
    arguments: &str,
    cancellation: McpCancellation,
    progress: Option<McpProgressSink>,
) -> ToolResult {
    let (mut result, _) = run_mcp_dispatch_with(
        manager,
        arguments,
        cancellation,
        progress,
        &RenderEnv::default(),
    )
    .await;
    result.output = limit_text(result.output, None).inline;
    result
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
        let mut blobs = Vec::new();
        let store = self.artifact_store.clone();
        self.refresh_mcp_secrets();
        let secrets = self.sensitive_values.0.clone();
        let mut result = if mode.allows_mutation() {
            let (sink, mut updates) = progress_channel();
            let env = RenderEnv {
                store: store.as_ref(),
                secrets: &secrets,
            };
            let dispatch = run_mcp_route(
                self.mcp.clone(),
                invocation.name,
                invocation.arguments,
                cancellation,
                Some(sink),
                &env,
            );
            tokio::pin!(dispatch);
            let mut progress_error = None;
            let mut updates_open = true;
            let result = loop {
                tokio::select! {
                    result = &mut dispatch => break result,
                    update = updates.recv(), if updates_open => {
                        let Some(update) = update else {
                            updates_open = false;
                            continue;
                        };
                        if progress_error.is_none() {
                            progress_error = self.push_mcp_progress(invocation, &update, &mut seq).err();
                        }
                    }
                }
            };
            while let Ok(update) = updates.try_recv() {
                if progress_error.is_none() {
                    progress_error = self.push_mcp_progress(invocation, &update, &mut seq).err();
                }
            }
            if let Some(error) = progress_error {
                return Err(error);
            }
            self.refresh_mcp_secrets();
            let (result, created) = result;
            blobs = created;
            result
        } else {
            ToolResult::fail(
                invocation.name,
                format!("{} is only available in Auto mode", invocation.name),
            )
        };
        blobs.extend(self.bound_mcp_output(&mut result));
        for handle in blobs {
            let carrier = ToolResult {
                artifact: Some(handle),
                ..ToolResult::ok("mcp", "")
            };
            self.record_existing_artifact(&carrier, &mut seq)?;
        }
        self.finish_tool(invocation, &mut result, started_at, &mut seq, None)?;
        Ok((result, seq))
    }

    /// Applies the inline budget to a result's text: the redacted text is
    /// stored whole and the model sees its head and tail with the artifact's
    /// pointer. Redaction comes first so the stored copy holds no secrets.
    /// Returns the artifact, which the caller announces.
    fn bound_mcp_output(&self, result: &mut ToolResult) -> Option<ArtifactHandle> {
        let redacted = self.redact_sensitive(&result.output);
        let limited = limit_text(redacted, self.artifact_store.as_ref());
        result.output = limited.inline;
        limited.artifact
    }

    /// Surfaces one server progress update as a transient, redacted tool
    /// progress event. A failed push cancels the call, as for native tools.
    fn push_mcp_progress(
        &mut self,
        invocation: ToolInvocation<'_>,
        update: &McpProgress,
        seq: &mut u64,
    ) -> Result<(), ProviderError> {
        let preview = self.redact_sensitive(&progress_preview(update));
        let pushed = push_runtime_transient_event(
            &mut self.app,
            seq,
            crate::EventKind::ToolProgress {
                batch_id: invocation.batch_id.into(),
                call_id: invocation.call_id.into(),
                name: invocation.name.into(),
                preview,
            },
        );
        if pushed.is_err() {
            if let Some(cancellation) = &self.cancellation {
                cancellation.cancel();
            }
        }
        pushed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn malformed_dispatch_arguments_are_rejected_before_discovery_or_sending() {
        let manager = Arc::new(McpManager::new(
            Default::default(),
            Default::default(),
            Default::default(),
        ));
        for args in [
            json!({"list":true, "server":42}),
            json!({"list":true, "unexpected":true}),
            json!({"list":true, "tool":false}),
            json!({"server":"fs", "tool":"write", "arguments":[]}),
        ] {
            let result = run_mcp_dispatch(
                Some(manager.clone()),
                &args.to_string(),
                McpCancellation::new(),
                None,
            )
            .await;
            assert!(!result.success, "accepted {args}: {}", result.output);
            assert!(
                result.output.contains("invalid mcp arguments"),
                "{}",
                result.output
            );
        }
    }

    fn update(progress: f64, total: Option<f64>, message: Option<&str>) -> McpProgress {
        McpProgress {
            progress,
            total,
            message: message.map(str::to_owned),
        }
    }

    #[test]
    fn progress_previews_are_one_line_with_optional_total_and_message() {
        assert_eq!(
            progress_preview(&update(3.0, Some(10.0), None)),
            "progress 3/10 (30%)"
        );
        assert_eq!(
            progress_preview(&update(0.5, None, Some("indexing"))),
            "progress 0.50 - indexing"
        );
        assert_eq!(
            progress_preview(&update(12.0, Some(0.0), None)),
            "progress 12"
        );
        assert_eq!(
            progress_preview(&update(20.0, Some(10.0), None)),
            "progress 20/10 (100%)"
        );
    }

    #[test]
    fn progress_sink_throttles_bursts_but_always_passes_completion() {
        let (sink, mut updates) = progress_channel();
        for step in 0..50 {
            sink(update(f64::from(step), Some(100.0), None));
        }
        let first = updates.try_recv().expect("first update passes");
        assert_eq!(first.progress, 0.0);
        assert!(
            updates.try_recv().is_err(),
            "updates inside the interval are dropped"
        );
        sink(update(100.0, Some(100.0), None));
        assert_eq!(
            updates.try_recv().expect("completion passes").progress,
            100.0
        );
    }

    #[test]
    fn progress_sink_never_queues_more_than_its_capacity() {
        let (sink, mut updates) = progress_channel();
        for step in 0..(PROGRESS_QUEUE_CAPACITY * 4) {
            sink(update(step as f64 + 1.0, Some(1.0), None));
        }
        let mut queued = 0;
        while updates.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(queued, PROGRESS_QUEUE_CAPACITY);
    }

    #[test]
    fn the_model_sees_content_blocks_and_codemode_keeps_the_structured_result() {
        // With content blocks the model gets the content only; the full
        // result (with `structuredContent`) stays available to codemode,
        // which reads the manager's raw value.
        let value = json!({"content":[{"type":"text","text":"human view"}],
            "structuredContent":{"rows":[1,2]}, "isError":false});
        let rendered = render_call_tool_result(&value, "s", "t", &RenderEnv::default());
        assert!(rendered.success);
        assert_eq!(rendered.text, "human view");
    }

    type Handler = Box<dyn Fn(&str, &Value) -> Result<Value, crate::mcp::McpError> + Send + Sync>;

    /// Connection whose answers come from a closure; records every request.
    struct ScriptedConnection {
        handler: Handler,
        calls: Mutex<Vec<(String, Value)>>,
        resources_stale: AtomicBool,
    }

    impl ScriptedConnection {
        fn new(
            handler: impl Fn(&str, &Value) -> Result<Value, crate::mcp::McpError>
                + Send
                + Sync
                + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                handler: Box::new(handler),
                calls: Mutex::new(Vec::new()),
                resources_stale: AtomicBool::new(false),
            })
        }

        fn count(&self, method: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(name, _)| name == method)
                .count()
        }

        fn params(&self, method: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(name, _)| name == method)
                .map(|(_, params)| params.clone())
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl crate::mcp::McpConnection for ScriptedConnection {
        async fn request(
            &self,
            method: &str,
            params: Value,
        ) -> Result<Value, crate::mcp::McpError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_owned(), params.clone()));
            (self.handler)(method, &params)
        }

        async fn notify(&self, _method: &str, _params: Value) {}

        fn is_closed(&self) -> bool {
            false
        }

        fn take_resources_stale(&self) -> bool {
            self.resources_stale.swap(false, Ordering::AcqRel)
        }
    }

    fn spec(name: &str) -> crate::mcp::McpServerSpec {
        crate::mcp::McpServerSpec::new(
            name,
            crate::mcp::McpTransport::Stdio {
                command: "unused".into(),
                args: Vec::new(),
                env: Default::default(),
            },
        )
    }

    fn manager_with(servers: Vec<(&str, Arc<ScriptedConnection>)>) -> Arc<McpManager> {
        let manager = Arc::new(McpManager::new(
            Default::default(),
            std::env::temp_dir(),
            Default::default(),
        ));
        for (name, connection) in servers {
            manager.insert_connection(spec(name), connection, Vec::new());
        }
        manager
    }

    async fn gateway(manager: &Arc<McpManager>, arguments: Value) -> ToolResult {
        run_mcp_dispatch(
            Some(manager.clone()),
            &arguments.to_string(),
            McpCancellation::new(),
            None,
        )
        .await
    }

    fn json_output(result: &ToolResult) -> Value {
        assert!(result.success, "{}", result.output);
        serde_json::from_str(&result.output).expect("listing is valid JSON")
    }

    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut value = 0_u32;
            for (index, byte) in chunk.iter().enumerate() {
                value |= u32::from(*byte) << (16 - 8 * index);
            }
            for index in 0..4 {
                if index <= chunk.len() {
                    out.push(ALPHABET[((value >> (18 - 6 * index)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn png_base64(width: u32, height: u32) -> String {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(width.to_be_bytes());
        png.extend(height.to_be_bytes());
        base64(&png)
    }

    /// Two-page resource server; `ui://` entries are mixed in.
    fn paged_server() -> Arc<ScriptedConnection> {
        ScriptedConnection::new(|method, params| match (method, params.get("cursor")) {
            ("resources/list", None) => Ok(json!({
                "resources": [
                    {"uri": "file:///a", "name": "a", "mimeType": "text/plain", "size": 3},
                    {"uri": "ui://widget", "name": "widget"},
                ],
                "nextCursor": "p2"
            })),
            ("resources/list", Some(cursor)) if cursor == "p2" => Ok(json!({
                "resources": [{"uri": "file:///b", "name": "b", "_meta": {"x": 1}}]
            })),
            ("resources/templates/list", None) => Ok(json!({
                "resourceTemplates": [{"uriTemplate": "file:///{path}", "name": "files"}]
            })),
            _ => Err(crate::mcp::McpError::Protocol(format!(
                "unexpected {method}"
            ))),
        })
    }

    #[tokio::test]
    async fn a_named_server_lists_one_page_and_continues_with_the_cursor() {
        let server = paged_server();
        let manager = manager_with(vec![("fs", server.clone())]);
        let first =
            json_output(&gateway(&manager, json!({"resources": true, "server": "fs"})).await);
        assert_eq!(first["server"], "fs");
        assert_eq!(first["nextCursor"], "p2");
        let entries = first["resources"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "ui:// entries are left out: {first}");
        assert_eq!(entries[0]["server"], "fs");
        assert_eq!(entries[0]["uri"], "file:///a");
        assert_eq!(entries[0]["size"], 3);
        let second = json_output(
            &gateway(
                &manager,
                json!({"resources": true, "server": "fs", "cursor": "p2"}),
            )
            .await,
        );
        assert_eq!(second["resources"][0]["uri"], "file:///b");
        assert!(second.get("nextCursor").is_none());
        assert!(second["resources"][0].get("_meta").is_none());
        assert_eq!(
            server.params("resources/list"),
            vec![json!({}), json!({"cursor": "p2"})]
        );
    }

    #[tokio::test]
    async fn templates_list_like_resources_and_a_server_without_them_has_none() {
        let manager = manager_with(vec![
            ("fs", paged_server()),
            (
                "bare",
                ScriptedConnection::new(|_, _| {
                    Err(crate::mcp::McpError::Server {
                        code: -32601,
                        message: "Method not found".into(),
                    })
                }),
            ),
        ]);
        let listed = json_output(
            &gateway(
                &manager,
                json!({"resource_templates": true, "server": "fs"}),
            )
            .await,
        );
        assert_eq!(
            listed["resourceTemplates"][0]["uriTemplate"],
            "file:///{path}"
        );
        assert!(listed["resourceTemplates"][0].get("uri").is_none());
        let none = json_output(
            &gateway(
                &manager,
                json!({"resource_templates": true, "server": "bare"}),
            )
            .await,
        );
        assert_eq!(none["resourceTemplates"], json!([]));
        let merged = json_output(&gateway(&manager, json!({"resource_templates": true})).await);
        assert_eq!(merged["resourceTemplates"].as_array().unwrap().len(), 1);
        assert!(merged.get("errors").is_none(), "{merged}");
    }

    #[tokio::test]
    async fn the_merged_listing_covers_connected_servers_and_names_the_failures() {
        let manager = manager_with(vec![
            ("zeta", paged_server()),
            (
                "broken",
                ScriptedConnection::new(|_, _| {
                    Err(crate::mcp::McpError::Protocol("backend is down".into()))
                }),
            ),
        ]);
        // A configured server that is not connected is reported, not started.
        manager.upsert(spec("idle"));
        let merged = json_output(&gateway(&manager, json!({"resources": true})).await);
        let entries = merged["resources"].as_array().unwrap();
        let uris: Vec<_> = entries
            .iter()
            .map(|entry| entry["uri"].as_str().unwrap())
            .collect();
        assert_eq!(
            uris,
            ["file:///a", "file:///b"],
            "every page of every server"
        );
        assert!(entries.iter().all(|entry| entry["server"] == "zeta"));
        assert_eq!(merged["errors"][0]["server"], "broken");
        assert!(merged["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("backend is down"));
        assert_eq!(merged["notConnected"], json!(["idle"]));
    }

    #[tokio::test]
    async fn complete_listings_are_cached_until_the_server_reports_a_change() {
        let server = paged_server();
        let manager = manager_with(vec![("fs", server.clone())]);
        assert_eq!(manager.cached_resource_counts("fs").resources, None);
        gateway(&manager, json!({"resources": true})).await;
        gateway(&manager, json!({"resources": true})).await;
        assert_eq!(server.count("resources/list"), 2, "two pages, fetched once");
        assert_eq!(manager.cached_resource_counts("fs").resources, Some(2));
        server.resources_stale.store(true, Ordering::Release);
        gateway(&manager, json!({"resources": true})).await;
        assert_eq!(
            server.count("resources/list"),
            4,
            "refetched after list_changed"
        );
        // Templates are cached independently of resources.
        gateway(&manager, json!({"resource_templates": true})).await;
        gateway(&manager, json!({"resource_templates": true})).await;
        assert_eq!(server.count("resources/templates/list"), 1);
        assert_eq!(manager.cached_resource_counts("fs").templates, Some(1));
        // A replaced connection starts with an empty cache.
        manager.insert_connection(spec("fs"), paged_server(), Vec::new());
        assert_eq!(manager.cached_resource_counts("fs").resources, None);
    }

    #[tokio::test]
    async fn listing_bounds_stop_runaway_servers_and_say_so() {
        let endless = ScriptedConnection::new(|_, params| {
            let page = params
                .get("cursor")
                .and_then(Value::as_str)
                .and_then(|cursor| cursor.strip_prefix('c'))
                .and_then(|number| number.parse::<usize>().ok())
                .unwrap_or(0);
            let resources: Vec<Value> = (0..100)
                .map(|index| json!({"uri": format!("file:///{page}/{index}"), "name": "n"}))
                .collect();
            Ok(json!({"resources": resources, "nextCursor": format!("c{}", page + 1)}))
        });
        let looping = ScriptedConnection::new(|_, _| {
            Ok(json!({"resources": [{"uri": "file:///x", "name": "x"}], "nextCursor": "same"}))
        });
        let manager = manager_with(vec![
            ("endless", endless.clone()),
            ("loop", looping.clone()),
        ]);
        let walked = manager
            .all_resources_cancellable("endless", McpCancellation::new())
            .await
            .into_result()
            .unwrap();
        assert_eq!(walked.items.len(), 1000);
        assert!(walked.truncated);
        assert_eq!(endless.count("resources/list"), 10);
        let walked = manager
            .all_resources_cancellable("loop", McpCancellation::new())
            .await
            .into_result()
            .unwrap();
        assert_eq!(walked.items.len(), 2);
        assert!(walked.truncated);
        assert_eq!(
            looping.count("resources/list"),
            2,
            "a repeated cursor ends the walk"
        );
        // The model's view stays valid JSON and says what it left out.
        let merged = json_output(&gateway(&manager, json!({"resources": true})).await);
        let shown = merged["resources"].as_array().unwrap().len() as u64;
        assert!(shown > 0 && shown < 1002, "{shown}");
        assert_eq!(merged["omitted"].as_u64().unwrap() + shown, 1002);
        assert_eq!(merged["truncated"], json!(["endless", "loop"]));
        assert!(
            gateway(&manager, json!({"resources": true}))
                .await
                .output
                .len()
                <= 12 * 1024
        );
        // Without an artifact store the complete listing cannot be pointed at.
        assert!(merged.get("complete").is_none());
    }

    #[tokio::test]
    async fn a_stored_listing_never_holds_registered_secrets() {
        let root = std::env::temp_dir().join(format!(
            "slim-mcp-listing-secret-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ArtifactStore::new(&root).unwrap();
        let echo = ScriptedConnection::new(|_, _| {
            let resources: Vec<Value> = (0..200)
                .map(|index| json!({"uri": format!("file:///{index}"), "name": "tok-listing-secret", "description": "d".repeat(80)}))
                .collect();
            Ok(json!({"resources": resources}))
        });
        let manager = manager_with(vec![("echo", echo)]);
        let secrets = vec!["tok-listing-secret".to_owned()];
        let env = RenderEnv {
            store: Some(&store),
            secrets: &secrets,
        };
        let (_, blobs) = run_mcp_dispatch_with(
            Some(manager),
            &json!({"resources": true, "server": "echo"}).to_string(),
            McpCancellation::new(),
            None,
            &env,
        )
        .await;
        assert_eq!(blobs.len(), 1);
        let stored = String::from_utf8(store.read(&blobs[0]).unwrap()).unwrap();
        assert!(!stored.contains("tok-listing-secret"));
        assert!(stored.contains("[REDACTED]"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn an_oversized_listing_is_stored_whole_and_pointed_at() {
        let root = std::env::temp_dir().join(format!(
            "slim-mcp-listing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ArtifactStore::new(&root).unwrap();
        let big = ScriptedConnection::new(|_, _| {
            let resources: Vec<Value> = (0..300)
                .map(|index| json!({"uri": format!("file:///dir/{index}"), "name": "entry", "description": "d".repeat(80)}))
                .collect();
            Ok(json!({"resources": resources, "nextCursor": "more"}))
        });
        let manager = manager_with(vec![("big", big)]);
        let env = RenderEnv {
            store: Some(&store),
            secrets: &[],
        };
        let (result, blobs) = run_mcp_dispatch_with(
            Some(manager.clone()),
            &json!({"resources": true, "server": "big"}).to_string(),
            McpCancellation::new(),
            None,
            &env,
        )
        .await;
        let shown: Value = serde_json::from_str(&result.output).expect("valid JSON");
        assert!(result.output.len() <= 12 * 1024);
        // The cursor still continues after the whole page, which is stored.
        assert_eq!(shown["nextCursor"], "more");
        assert_eq!(blobs.len(), 1);
        let stored: Value = serde_json::from_slice(&store.read(&blobs[0]).unwrap()).unwrap();
        assert_eq!(stored["resources"].as_array().unwrap().len(), 300);
        assert_eq!(
            shown["resources"].as_array().unwrap().len() as u64
                + shown["omitted"].as_u64().unwrap(),
            300
        );
        assert!(shown["complete"].as_str().unwrap().contains(&blobs[0].id));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reads_render_text_images_and_labelled_contents() {
        let encoded = png_base64(8, 8);
        let server = ScriptedConnection::new(move |method, params| {
            assert_eq!(method, "resources/read");
            match params["uri"].as_str().unwrap() {
                "file:///one" => Ok(json!({"contents": [{"uri": "file:///one", "text": "hello"}]})),
                "file:///dir" => Ok(json!({"contents": [
                    {"uri": "file:///dir/a", "text": "A"},
                    {"uri": "file:///dir/pic", "mimeType": "image/png", "blob": encoded},
                ]})),
                "file:///none" => Ok(json!({"contents": []})),
                _ => Err(crate::mcp::McpError::Server {
                    code: -32002,
                    message: "Resource not found".into(),
                }),
            }
        });
        let manager = manager_with(vec![("fs", server.clone())]);
        let one = gateway(&manager, json!({"server": "fs", "uri": " file:///one "})).await;
        assert!(one.success && one.media.is_empty());
        assert_eq!(one.output, "hello");
        assert_eq!(
            server.params("resources/read"),
            vec![json!({"uri": "file:///one"})]
        );
        let dir = gateway(&manager, json!({"server": "fs", "uri": "file:///dir"})).await;
        assert!(dir.success, "{}", dir.output);
        assert!(
            dir.output.starts_with(
                "file:///dir/a:\nA\nfile:///dir/pic:\n[image file:///dir/pic (image/png), "
            ),
            "{}",
            dir.output
        );
        assert_eq!(dir.media.len(), 1);
        let none = gateway(&manager, json!({"server": "fs", "uri": "file:///none"})).await;
        assert_eq!(none.output, "Resource file:///none is empty.");
        let missing = gateway(&manager, json!({"server": "fs", "uri": "file:///gone"})).await;
        assert!(!missing.success);
        assert!(
            missing.output.contains("Resource not found"),
            "{}",
            missing.output
        );
    }

    #[tokio::test]
    async fn resource_requests_are_validated_before_any_server_is_asked() {
        let server = paged_server();
        let manager = manager_with(vec![("fs", server.clone())]);
        for arguments in [
            json!({"resources": true, "resource_templates": true}),
            json!({"resources": true, "cursor": "p2"}),
            json!({"resources": true, "list": true}),
            json!({"resources": true, "query": "x"}),
            json!({"uri": "file:///a"}),
            json!({"server": "fs", "uri": "file:///a", "tool": "t"}),
            json!({"cursor": "c"}),
            json!({"server": "fs", "uri": "  "}),
            json!({"server": "fs", "uri": "u".repeat(5000)}),
        ] {
            let result = gateway(&manager, arguments.clone()).await;
            assert!(!result.success, "accepted {arguments}");
            assert!(result.output.starts_with("mcp error:"), "{}", result.output);
        }
        let unknown = gateway(&manager, json!({"resources": true, "server": "nope"})).await;
        assert!(
            unknown.output.contains("unknown MCP server: nope"),
            "{}",
            unknown.output
        );
        assert!(
            server.calls.lock().unwrap().is_empty(),
            "nothing reached the server"
        );
    }

    #[tokio::test]
    async fn a_hidden_server_is_unreachable_for_resources() {
        let manager = manager_with(vec![]);
        let mut hidden = spec("secret");
        hidden.options.exposure = crate::mcp::McpExposure::Hidden;
        let server = paged_server();
        manager.insert_connection(hidden, server.clone(), Vec::new());
        let named = gateway(&manager, json!({"resources": true, "server": "secret"})).await;
        assert!(!named.success);
        assert!(named.output.contains("hidden"), "{}", named.output);
        let read = gateway(&manager, json!({"server": "secret", "uri": "file:///a"})).await;
        assert!(!read.success);
        let merged = json_output(&gateway(&manager, json!({"resources": true})).await);
        assert_eq!(merged["resources"], json!([]));
        assert!(server.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tool_results_carry_links_images_and_audio_placeholders_through_the_gateway() {
        let picture = png_base64(16, 16);
        let server = ScriptedConnection::new(move |method, _| {
            assert_eq!(method, "tools/call");
            Ok(json!({"content": [
                {"type": "text", "text": "see"},
                {"type": "image", "mimeType": "image/png", "data": picture},
                {"type": "resource_link", "uri": "file:///r", "name": "r"},
                {"type": "audio", "mimeType": "audio/mpeg", "data": "AAAA"},
            ], "structuredContent": {"ignored": true}}))
        });
        let manager = manager_with(vec![("fs", server)]);
        let result = gateway(
            &manager,
            json!({"server": "fs", "tool": "shot", "arguments": {}}),
        )
        .await;
        assert!(result.success);
        assert_eq!(result.media.len(), 1);
        let lines: Vec<&str> = result.output.lines().collect();
        assert_eq!(lines[0], "see");
        assert!(lines[1].starts_with("[image image/png, "), "{}", lines[1]);
        assert_eq!(
            lines[2],
            "[Resource file:///r \"r\". Read it with mcp {server:\"fs\", uri:\"file:///r\"}]"
        );
        assert_eq!(lines[3], "[audio audio/mpeg omitted]");
        assert!(!result.output.contains("ignored"));
    }

    #[test]
    fn text_only_results_keep_the_existing_error_and_output_contract() {
        let rendered = render_call_tool_result(
            &json!({"content":[{"type":"text","text":"tool failed"}], "isError":true}),
            "s",
            "t",
            &RenderEnv::default(),
        );
        assert!(!rendered.success);
        assert_eq!(rendered.text, "tool failed");
    }
}
