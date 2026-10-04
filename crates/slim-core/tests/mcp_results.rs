//! MCP resources and result rendering, end to end: the stdio resource
//! server fixture against the manager (capabilities, paging, the cache that
//! `resources/list_changed` invalidates), image content blocks on tool
//! messages for each provider wire, and oversized results spilled to
//! artifacts.

#[path = "../../../tests/support/http_fixture.rs"]
mod http_fixture;
#[path = "../../../tests/support/temp_root.rs"]
mod temp_root;

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use http_fixture::{accept_within, bind_listener, read_http_request, write_sse};
use serde_json::{json, Value};
use slim_core::mcp::{
    McpCancellation, McpConnection, McpError, McpManager, McpServerSpec, McpToolSummary,
    McpTransport,
};
use slim_core::process::ExecutableResolver;
use slim_core::provider::{
    AnthropicAdapter, HttpProviderClient, OpenAiCodexAdapter, OpenAiCompatibleAdapter,
    ProviderAdapter, ProviderConfig, ProviderContentBlock, ProviderMessage, ProviderToolCall,
};
use slim_core::runtime::AgentLoopConfig;
use slim_core::{OperatingMode, Runtime};
use temp_root::TempRoot;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    encode_base64(&bytes)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("tokio runtime")
}

// ---------------------------------------------------------------------------
// Stdio resource server
// ---------------------------------------------------------------------------

/// Resource server over stdio. Every request is appended to
/// `$MCP_RES_DIR/messages.jsonl`. `initialize` reports `$MCP_RES_CAPS`.
/// `resources/list` has two pages (`p2`) plus a `ui://` entry that must stay
/// hidden; once `$MCP_RES_DIR/changed` exists it lists a single other entry.
/// Reading `file:///change` creates that file and announces the change.
#[test]
#[ignore = "subprocess fixture"]
fn mcp_resources_fixture() {
    let dir = PathBuf::from(std::env::var_os("MCP_RES_DIR").expect("fixture dir"));
    let capabilities: Value = serde_json::from_str(
        &std::env::var("MCP_RES_CAPS").unwrap_or_else(|_| r#"{"resources":{}}"#.into()),
    )
    .expect("caps json");
    let no_templates = std::env::var_os("MCP_RES_NO_TEMPLATES").is_some();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let write_line = |value: &Value| {
        let mut out = stdout.lock();
        out.write_all(value.to_string().as_bytes()).expect("write");
        out.write_all(b"\n").expect("newline");
        out.flush().expect("flush");
    };
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("messages.jsonl"))
            .expect("open messages log");
        writeln!(log, "{message}").expect("record message");
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let params = &message["params"];
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let error = |code: i64, text: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}});
        let response = match method {
            "initialize" => reply(json!({
                "protocolVersion": "2025-11-25",
                "capabilities": capabilities,
                "serverInfo": {"name": "resource-fixture", "version": "1"},
            })),
            "resources/list" if dir.join("changed").exists() => reply(json!({
                "resources": [{"uri": "file:///c", "name": "c"}]
            })),
            "resources/list" => match params.get("cursor").and_then(Value::as_str) {
                None => reply(json!({
                    "resources": [
                        {"uri": "file:///a", "name": "a", "mimeType": "text/plain"},
                        {"uri": "ui://app", "name": "app"},
                    ],
                    "nextCursor": "p2"
                })),
                Some("p2") => reply(json!({"resources": [{"uri": "file:///b", "name": "b"}]})),
                Some(_) => error(-32602, "bad cursor"),
            },
            "resources/templates/list" if no_templates => error(-32601, "Method not found"),
            "resources/templates/list" => reply(json!({
                "resourceTemplates": [{"uriTemplate": "file:///{name}", "name": "files"}]
            })),
            "resources/read" => match params["uri"].as_str().unwrap_or_default() {
                "file:///text" => {
                    reply(json!({"contents": [{"uri": "file:///text", "text": "hello"}]}))
                }
                "file:///blob" => reply(json!({"contents": [{
                    "uri": "file:///blob", "mimeType": "application/octet-stream",
                    "blob": "AAEC/w=="
                }]})),
                "file:///change" => {
                    std::fs::write(dir.join("changed"), "").expect("mark changed");
                    write_line(&reply(
                        json!({"contents": [{"uri": "file:///change", "text": "ok"}]}),
                    ));
                    write_line(&json!({
                        "jsonrpc": "2.0", "method": "notifications/resources/list_changed"
                    }));
                    continue;
                }
                _ => error(-32002, "Resource not found"),
            },
            "tools/list" => reply(json!({"tools": []})),
            _ => error(-32601, "Method not found"),
        };
        write_line(&response);
    }
}

struct Fixture {
    dir: TempRoot,
    manager: Arc<McpManager>,
}

impl Fixture {
    fn new(label: &str, capabilities: &str, no_templates: bool) -> Self {
        let dir = TempRoot::new(label);
        let mut env = BTreeMap::from([
            (
                "MCP_RES_DIR".to_owned(),
                dir.path().to_string_lossy().into_owned(),
            ),
            ("MCP_RES_CAPS".to_owned(), capabilities.to_owned()),
        ]);
        if no_templates {
            env.insert("MCP_RES_NO_TEMPLATES".into(), "1".into());
        }
        let exe = std::env::current_exe().expect("current exe");
        let mut spec = McpServerSpec::new(
            "res",
            McpTransport::Stdio {
                command: exe.to_string_lossy().into_owned(),
                args: vec![
                    "--exact".into(),
                    "mcp_resources_fixture".into(),
                    "--ignored".into(),
                ],
                env,
            },
        );
        spec.timeout = Duration::from_secs(15);
        let manager = Arc::new(McpManager::new(
            BTreeMap::from([(spec.name.clone(), spec)]),
            dir.path().to_path_buf(),
            ExecutableResolver::default(),
        ));
        Self { dir, manager }
    }

    fn methods(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("messages.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|message| message["method"].as_str().map(str::to_owned))
            .collect()
    }

    fn count(&self, method: &str) -> usize {
        self.methods()
            .iter()
            .filter(|name| name.as_str() == method)
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        runtime().block_on(self.manager.disconnect_all());
    }
}

#[test]
fn a_resource_only_server_lists_every_page_without_ever_asking_for_tools() {
    let fixture = Fixture::new("res-list", r#"{"resources":{}}"#, false);
    let listing = runtime()
        .block_on(
            fixture
                .manager
                .all_resources_cancellable("res", McpCancellation::new()),
        )
        .into_result()
        .expect("listing");
    let uris: Vec<_> = listing.items.iter().map(|item| item.uri.as_str()).collect();
    assert_eq!(
        uris,
        ["file:///a", "file:///b"],
        "the ui:// entry is hidden"
    );
    assert!(!listing.truncated);
    assert_eq!(listing.items[0].mime_type.as_deref(), Some("text/plain"));
    assert_eq!(fixture.count("resources/list"), 2);
    assert_eq!(
        fixture.count("tools/list"),
        0,
        "no tools capability, no tools/list"
    );
    // The complete listing is cached for this connection.
    runtime()
        .block_on(
            fixture
                .manager
                .all_resources_cancellable("res", McpCancellation::new()),
        )
        .into_result()
        .expect("cached listing");
    assert_eq!(fixture.count("resources/list"), 2);
    assert_eq!(
        fixture.manager.cached_resource_counts("res").resources,
        Some(2)
    );
}

#[test]
fn a_single_page_continues_with_the_servers_cursor() {
    let fixture = Fixture::new("res-page", r#"{"resources":{}}"#, false);
    let first = runtime()
        .block_on(
            fixture
                .manager
                .resources_page_cancellable("res", None, McpCancellation::new()),
        )
        .into_result()
        .expect("first page");
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.next_cursor.as_deref(), Some("p2"));
    let second = runtime()
        .block_on(fixture.manager.resources_page_cancellable(
            "res",
            first.next_cursor.as_deref(),
            McpCancellation::new(),
        ))
        .into_result()
        .expect("second page");
    assert_eq!(second.items[0].uri, "file:///b");
    assert_eq!(second.next_cursor, None);
}

#[test]
fn templates_list_and_a_server_that_lacks_them_has_none() {
    let with = Fixture::new("res-templates", r#"{"resources":{}}"#, false);
    let listed = runtime()
        .block_on(
            with.manager
                .all_resource_templates_cancellable("res", McpCancellation::new()),
        )
        .into_result()
        .expect("templates");
    assert_eq!(listed.items[0].uri, "file:///{name}");
    let without = Fixture::new("res-no-templates", r#"{"resources":{}}"#, true);
    let none = runtime()
        .block_on(
            without
                .manager
                .all_resource_templates_cancellable("res", McpCancellation::new()),
        )
        .into_result()
        .expect("method not found means no templates");
    assert!(none.items.is_empty());
    let page = runtime()
        .block_on(without.manager.resource_templates_page_cancellable(
            "res",
            None,
            McpCancellation::new(),
        ))
        .into_result()
        .expect("empty page");
    assert!(page.items.is_empty() && page.next_cursor.is_none());
}

#[test]
fn a_server_without_the_resources_capability_is_refused_before_any_request() {
    let fixture = Fixture::new("res-no-cap", r#"{"tools":{}}"#, false);
    let error = runtime()
        .block_on(
            fixture
                .manager
                .all_resources_cancellable("res", McpCancellation::new()),
        )
        .into_result()
        .expect_err("no resources capability");
    assert!(
        error.to_string().contains("does not offer resources"),
        "{error}"
    );
    let read = runtime()
        .block_on(fixture.manager.read_resource_cancellable(
            "res",
            "file:///text",
            McpCancellation::new(),
        ))
        .into_result()
        .expect_err("no resources capability");
    assert!(read.to_string().contains("does not offer resources"));
    assert_eq!(fixture.count("resources/list"), 0);
    assert_eq!(fixture.count("resources/read"), 0);
    // A merged listing does not even try it.
    let targets = fixture.manager.resource_targets();
    assert!(targets.listable.is_empty(), "{targets:?}");
}

#[test]
fn reads_return_the_servers_contents_and_errors_stay_errors() {
    let fixture = Fixture::new("res-read", r#"{"resources":{}}"#, false);
    let text = runtime()
        .block_on(fixture.manager.read_resource_cancellable(
            "res",
            "file:///text",
            McpCancellation::new(),
        ))
        .into_result()
        .expect("read");
    assert_eq!(text["contents"][0]["text"], "hello");
    let blob = runtime()
        .block_on(fixture.manager.read_resource_cancellable(
            "res",
            "file:///blob",
            McpCancellation::new(),
        ))
        .into_result()
        .expect("blob");
    assert_eq!(blob["contents"][0]["blob"], "AAEC/w==");
    let missing = runtime()
        .block_on(fixture.manager.read_resource_cancellable(
            "res",
            "file:///nope",
            McpCancellation::new(),
        ))
        .into_result()
        .expect_err("not found");
    assert!(
        missing.to_string().contains("Resource not found"),
        "{missing}"
    );
    let empty = runtime()
        .block_on(
            fixture
                .manager
                .read_resource_cancellable("res", "   ", McpCancellation::new()),
        )
        .into_result()
        .expect_err("empty uri");
    assert!(empty.to_string().contains("uri"), "{empty}");
    // The connection survives every one of those.
    assert_eq!(fixture.count("initialize"), 1);
}

#[test]
fn list_changed_invalidates_the_cached_listing() {
    let fixture = Fixture::new("res-stale", r#"{"resources":{"listChanged":true}}"#, false);
    let before = runtime()
        .block_on(
            fixture
                .manager
                .all_resources_cancellable("res", McpCancellation::new()),
        )
        .into_result()
        .expect("listing");
    assert_eq!(before.items.len(), 2);
    runtime()
        .block_on(fixture.manager.read_resource_cancellable(
            "res",
            "file:///change",
            McpCancellation::new(),
        ))
        .into_result()
        .expect("trigger");
    // The announcement arrives on the connection's reader thread.
    let deadline = Instant::now() + Duration::from_secs(10);
    let after = loop {
        let listing = runtime()
            .block_on(
                fixture
                    .manager
                    .all_resources_cancellable("res", McpCancellation::new()),
            )
            .into_result()
            .expect("listing");
        if listing.items.len() == 1 {
            break listing;
        }
        assert!(Instant::now() < deadline, "listing never refreshed");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(after.items[0].uri, "file:///c");
    assert_eq!(
        fixture.manager.cached_resource_counts("res").resources,
        Some(1)
    );
}

// ---------------------------------------------------------------------------
// Provider wires
// ---------------------------------------------------------------------------

fn image_tool_conversation() -> Vec<ProviderMessage> {
    vec![
        ProviderMessage::user("look"),
        ProviderMessage::assistant(
            "",
            vec![ProviderToolCall {
                id: "call-1".into(),
                name: "mcp".into(),
                arguments: r#"{"server":"s","tool":"shot"}"#.into(),
            }],
        ),
        ProviderMessage::tool("mcp", "call-1", "[image image/png, 24 B]").with_content_blocks(
            vec![ProviderContentBlock::image("image/png", png_base64(4, 4))],
        ),
    ]
}

#[test]
fn only_the_anthropic_and_responses_wires_accept_tool_result_images() {
    let anthropic =
        AnthropicAdapter::new(ProviderConfig::anthropic("http://x", "claude", "k")).unwrap();
    let codex = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://chatgpt.com/backend-api",
        "gpt-5.6-terra",
        "token",
        "account",
    ))
    .unwrap();
    let chat =
        OpenAiCompatibleAdapter::new(ProviderConfig::openai("http://x", "model", "k")).unwrap();
    assert!(anthropic.accepts_tool_result_images());
    assert!(codex.accepts_tool_result_images());
    assert!(!chat.accepts_tool_result_images());
}

#[test]
fn anthropic_tool_results_carry_image_blocks() {
    let adapter =
        AnthropicAdapter::new(ProviderConfig::anthropic("http://x", "claude", "k")).unwrap();
    let request = adapter
        .build_messages_request_with_tools_checked(&image_tool_conversation(), &[])
        .expect("valid request");
    let body: Value = serde_json::from_str(&request.body).unwrap();
    let result = &body["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "call-1");
    let content = result["content"].as_array().expect("block content");
    assert_eq!(
        content[0],
        json!({"type": "text", "text": "[image image/png, 24 B]"})
    );
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert_eq!(content[1]["source"]["data"], png_base64(4, 4));
    // Text-only results keep the string form.
    let plain = adapter
        .build_messages_request_with_tools_checked(
            &[
                ProviderMessage::user("x"),
                ProviderMessage::tool("mcp", "call-2", "just text"),
            ],
            &[],
        )
        .unwrap();
    let plain: Value = serde_json::from_str(&plain.body).unwrap();
    assert_eq!(plain["messages"][1]["content"][0]["content"], "just text");
}

#[test]
fn responses_tool_outputs_carry_input_images() {
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://chatgpt.com/backend-api",
        "gpt-5.6-terra",
        "token",
        "account",
    ))
    .unwrap();
    let request = adapter.build_messages_request_with_tools(&image_tool_conversation(), &[]);
    let body: Value = serde_json::from_str(&request.body).unwrap();
    let output = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("function_call_output");
    let parts = output["output"].as_array().expect("array output");
    assert_eq!(
        parts[0],
        json!({"type": "input_text", "text": "[image image/png, 24 B]"})
    );
    assert_eq!(parts[1]["type"], "input_image");
    assert!(parts[1]["image_url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    let plain = adapter.build_messages_request_with_tools(
        &[
            ProviderMessage::user("x"),
            ProviderMessage::tool("mcp", "call-2", "just text"),
        ],
        &[],
    );
    let plain: Value = serde_json::from_str(&plain.body).unwrap();
    let item = plain["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(item["output"], "just text");
}

#[test]
fn chat_tool_messages_stay_plain_text_even_with_media() {
    let adapter =
        OpenAiCompatibleAdapter::new(ProviderConfig::openai("http://x", "model", "k")).unwrap();
    let request = adapter.build_messages_request_with_tools(&image_tool_conversation(), &[]);
    let body: Value = serde_json::from_str(&request.body).unwrap();
    let tool = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    let content = tool["content"].as_str().expect("string content");
    assert!(
        content.starts_with("[image image/png, 24 B]\n[image image/png omitted"),
        "{content}"
    );
    assert!(!request.body.contains("image_url"));
}

// ---------------------------------------------------------------------------
// Agent loop: result rendering through each wire
// ---------------------------------------------------------------------------

/// MCP connection whose tool returns what the test scripted.
struct ToolServer {
    result: Value,
    closed: AtomicBool,
    calls: Mutex<Vec<Value>>,
}

#[async_trait::async_trait]
impl McpConnection for ToolServer {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        assert_eq!(method, "tools/call", "unexpected {method}");
        self.calls.lock().unwrap().push(params);
        Ok(self.result.clone())
    }

    async fn notify(&self, _method: &str, _params: Value) {}

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

fn mcp_runtime(result: Value, artifacts: Option<&std::path::Path>) -> Runtime {
    let manager = Arc::new(McpManager::new(
        BTreeMap::new(),
        std::env::temp_dir(),
        ExecutableResolver::default(),
    ));
    manager.insert_connection(
        McpServerSpec::new(
            "srv",
            McpTransport::Stdio {
                command: "unused".into(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
        ),
        Arc::new(ToolServer {
            result,
            closed: AtomicBool::new(false),
            calls: Mutex::new(Vec::new()),
        }),
        vec![McpToolSummary {
            name: "shot".into(),
            description: None,
            schema: json!({"type": "object"}),
            output_schema: None,
        }],
    );
    let mut runtime = match artifacts {
        Some(root) => Runtime::with_artifact_store(root).expect("artifact store"),
        None => Runtime::new(),
    };
    runtime.set_mcp_manager(Some(manager));
    runtime
}

fn run_loop<A: ProviderAdapter + Send + Sync + 'static>(
    runtime: &mut Runtime,
    client: &HttpProviderClient<A>,
    cwd: &std::path::Path,
) -> slim_core::runtime::AgentLoopResult {
    self::runtime()
        .block_on(runtime.run_agent_loop(
            client,
            "take a screenshot",
            OperatingMode::Auto,
            cwd,
            1,
            AgentLoopConfig {
                max_turns: 3,
                ..AgentLoopConfig::default()
            },
        ))
        .expect("loop")
}

const MCP_CALL_ARGUMENTS: &str = r#"{"server":"srv","tool":"shot","arguments":{}}"#;

fn anthropic_tool_sse() -> String {
    [
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "tool_use", "id": "toolu-1", "name": "mcp"}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"partial_json": MCP_CALL_ARGUMENTS}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        json!({"type": "message_stop"}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

fn anthropic_text_sse(text: &str) -> String {
    [
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        json!({"type": "message_stop"}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

fn chat_tool_sse() -> String {
    let call = json!({"choices": [{"delta": {"tool_calls": [{
        "index": 0, "id": "call-1",
        "function": {"name": "mcp", "arguments": MCP_CALL_ARGUMENTS}
    }]}, "finish_reason": "tool_calls"}]});
    format!("data: {call}\n\ndata: [DONE]\n\n")
}

fn chat_text_sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]})
    )
}

/// Serves `responses` in order and returns the request bodies.
fn serve(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<Value>>) {
    let (listener, address) = bind_listener();
    let endpoint = format!("http://{address}");
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for body in responses {
            let mut stream = accept_within(&listener, Duration::from_secs(10));
            let request = read_http_request(&mut stream);
            let (_, payload) = request.split_once("\r\n\r\n").expect("request body");
            requests.push(serde_json::from_str(payload).expect("request JSON"));
            write_sse(&mut stream, &body);
        }
        requests
    });
    (endpoint, server)
}

fn screenshot_result() -> Value {
    json!({
        "content": [
            {"type": "text", "text": "page loaded"},
            {"type": "image", "mimeType": "image/png", "data": png_base64(64, 48)},
        ],
        "structuredContent": {"ignored": "by the model"},
    })
}

#[test]
fn an_mcp_image_reaches_an_anthropic_model_inside_the_tool_result() {
    let root = TempRoot::new("mcp-image-anthropic");
    let (endpoint, server) = serve(vec![anthropic_tool_sse(), anthropic_text_sse("seen")]);
    let client = HttpProviderClient::new(
        AnthropicAdapter::new(ProviderConfig::anthropic(&endpoint, "claude-fixture", "k")).unwrap(),
        Duration::from_secs(5),
    )
    .unwrap();
    let mut runtime = mcp_runtime(screenshot_result(), None);
    let result = run_loop(&mut runtime, &client, &root);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(result.tool_results[0].success);
    assert!(!result.tool_results[0].output.contains("ignored"));
    let tool_result = &requests[1]["messages"][2]["content"][0];
    assert_eq!(tool_result["tool_use_id"], "toolu-1");
    let parts = tool_result["content"].as_array().expect("block content");
    assert_eq!(parts[0]["type"], "text");
    assert!(parts[0]["text"]
        .as_str()
        .unwrap()
        .starts_with("page loaded\n[image image/png, "));
    assert_eq!(parts[1]["type"], "image");
    assert_eq!(parts[1]["source"]["data"], png_base64(64, 48));
}

#[test]
fn an_mcp_image_is_withheld_from_a_chat_model_and_the_text_says_so() {
    let root = TempRoot::new("mcp-image-chat");
    let (endpoint, server) = serve(vec![chat_tool_sse(), chat_text_sse("ok")]);
    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(&endpoint, "fixture-model", "k"))
            .unwrap(),
        Duration::from_secs(5),
    )
    .unwrap();
    let mut runtime = mcp_runtime(screenshot_result(), None);
    let result = run_loop(&mut runtime, &client, &root);
    let requests = server.join().unwrap();
    assert!(result.tool_results[0].success);
    let wire = requests[1].to_string();
    assert!(!wire.contains("image_url"), "{wire}");
    assert!(
        !wire.contains(&png_base64(64, 48)),
        "image bytes never reach a chat wire"
    );
    let tool = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    let content = tool["content"].as_str().unwrap();
    assert!(content.contains("page loaded"), "{content}");
    assert!(
        content
            .contains("1 image(s) not shown: this provider does not accept images in tool results"),
        "{content}"
    );
}

#[test]
fn oversized_results_are_stored_whole_and_the_model_sees_both_ends() {
    let root = TempRoot::new("mcp-spill");
    let artifacts = root.path().join("artifacts");
    let big = format!("BEGIN-{}-END", "é".repeat(60_000));
    let (endpoint, server) = serve(vec![chat_tool_sse(), chat_text_sse("done")]);
    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(&endpoint, "fixture-model", "k"))
            .unwrap(),
        Duration::from_secs(5),
    )
    .unwrap();
    let mut runtime = mcp_runtime(
        json!({"content": [{"type": "text", "text": big}]}),
        Some(&artifacts),
    );
    let result = run_loop(&mut runtime, &client, root.path());
    let requests = server.join().unwrap();
    let outcome = &result.tool_results[0];
    assert!(outcome.success);
    assert!(
        outcome.output.len() <= 14 * 1024,
        "{}",
        outcome.output.len()
    );
    assert!(outcome.output.starts_with("BEGIN-") && outcome.output.ends_with("-END"));
    assert!(outcome.output.contains("bytes omitted from the middle"));
    let stored: Vec<_> = std::fs::read_dir(&artifacts)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(stored.len(), 1, "the complete output is the only artifact");
    assert_eq!(std::fs::read_to_string(stored[0].path()).unwrap(), big);
    let id = stored[0].file_name().to_string_lossy().into_owned();
    assert!(
        outcome.output.contains(&id),
        "the output names its artifact"
    );
    assert!(runtime.app.events().iter().any(|event| matches!(
        &event.kind,
        slim_core::EventKind::ArtifactStored { id: stored_id, .. } if *stored_id == id
    )));
    // The request carries the head and tail and the way to the artifact.
    let tool = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    let content = tool["content"].as_str().unwrap();
    assert!(
        content.starts_with("BEGIN-") && content.contains("-END"),
        "{}",
        &content[..content.len().min(300)]
    );
    assert!(content.contains(&id), "{content}");
    assert!(content.len() < 16 * 1024);
}

#[test]
fn binary_resource_blobs_become_artifacts_the_text_points_to() {
    let root = TempRoot::new("mcp-blob");
    let artifacts = root.path().join("artifacts");
    let (endpoint, server) = serve(vec![chat_tool_sse(), chat_text_sse("done")]);
    let client = HttpProviderClient::new(
        OpenAiCompatibleAdapter::new(ProviderConfig::openai(&endpoint, "fixture-model", "k"))
            .unwrap(),
        Duration::from_secs(5),
    )
    .unwrap();
    let mut runtime = mcp_runtime(
        json!({"content": [
            {"type": "resource", "resource": {
                "uri": "file:///data.bin", "mimeType": "application/octet-stream",
                "blob": encode_base64(&[0, 1, 2, 3, 255])
            }},
            {"type": "resource", "resource": {
                "uri": "file:///data.json", "mimeType": "application/json",
                "blob": encode_base64(b"{\"k\":1}")
            }},
        ]}),
        Some(&artifacts),
    );
    let result = run_loop(&mut runtime, &client, root.path());
    let _ = server.join().unwrap();
    let output = &result.tool_results[0].output;
    assert!(
        output.contains("[Binary resource file:///data.bin (application/octet-stream, 5 B) saved to artifact id=mcp-resource-"),
        "{output}"
    );
    assert!(
        output.contains("{\"k\":1}"),
        "text-like blobs are decoded: {output}"
    );
    let stored: Vec<_> = std::fs::read_dir(&artifacts)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        std::fs::read(stored[0].path()).unwrap(),
        vec![0, 1, 2, 3, 255]
    );
    // The artifact is announced like any other, so the session can find it.
    assert!(runtime.app.events().iter().any(|event| matches!(
        &event.kind,
        slim_core::EventKind::ArtifactStored { id, .. } if id.starts_with("mcp-resource-")
    )));
}
