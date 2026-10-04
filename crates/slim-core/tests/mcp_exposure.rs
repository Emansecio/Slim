//! Exposure of MCP tools (M6): `gateway` / `direct` / `hidden` per server and
//! per tool, provider tool naming, BM25 discovery, server listings with
//! description and instructions, the awareness block, and the end-to-end
//! path of a direct call through the agent loop.

#[path = "../../../tests/support/http_fixture.rs"]
mod http_fixture;
#[path = "../../../tests/support/mcp_http_mock.rs"]
mod mock;
#[path = "../../../tests/support/temp_root.rs"]
mod temp_root;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use http_fixture::{accept_within, bind_listener, read_http_request, write_sse, SSE_FINAL_ANSWER};
use serde_json::{json, Value};
use slim_core::mcp::{
    McpCancellation, McpCleanupStatus, McpConnection, McpError, McpExposure, McpInterruption,
    McpManager, McpProgress, McpProgressSink, McpRequestOutcome, McpServerBlock,
    McpServerHandshake, McpServerSpec, McpToolSummary, McpTransport, MAX_AWARENESS_BYTES,
    MAX_DIRECT_TOOLS, MAX_PROVIDER_TOOL_NAME,
};
use slim_core::process::ExecutableResolver;
use slim_core::provider::{HttpProviderClient, OpenAiCompatibleAdapter, ProviderConfig};
use slim_core::runtime::CancellationToken;
use slim_core::EventKind;
use slim_core::{AgentLoopConfig, AgentLoopStop, OperatingMode, Runtime};
use temp_root::TempRoot;

struct Fake {
    calls: Mutex<Vec<(String, Value)>>,
    closed: AtomicBool,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        })
    }

    fn tool_calls(&self) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "tools/call")
            .map(|(_, params)| params.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl McpConnection for Fake {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_owned(), params.clone()));
        match method {
            "tools/call" => Ok(json!({
                "content": [{"type": "text", "text": format!("pong:{}", params["name"].as_str().unwrap_or(""))}],
            })),
            other => Err(McpError::Protocol(format!("unexpected method {other}"))),
        }
    }

    async fn notify(&self, _method: &str, _params: Value) {}

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

fn spec(name: &str, exposure: McpExposure, overrides: &[(&str, McpExposure)]) -> McpServerSpec {
    let mut spec = McpServerSpec::new(
        name,
        McpTransport::Stdio {
            command: "controlled-test-connection".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        },
    );
    spec.timeout = Duration::from_secs(5);
    spec.options.exposure = exposure;
    spec.options.tool_exposure = overrides
        .iter()
        .map(|(pattern, exposure)| ((*pattern).to_owned(), *exposure))
        .collect();
    spec
}

fn tool(name: &str, description: &str, schema: Value) -> McpToolSummary {
    McpToolSummary {
        name: name.into(),
        description: Some(description.into()),
        schema,
        output_schema: None,
    }
}

fn plain_tool(name: &str) -> McpToolSummary {
    tool(name, &format!("Does {name}"), json!({"type": "object"}))
}

fn manager() -> Arc<McpManager> {
    Arc::new(McpManager::new(
        BTreeMap::new(),
        PathBuf::from("."),
        ExecutableResolver::default(),
    ))
}

fn names(manager: &McpManager) -> Vec<String> {
    manager
        .direct_tool_definitions()
        .iter()
        .map(|definition| definition["name"].as_str().unwrap().to_owned())
        .collect()
}

fn handshake(instructions: Option<&str>) -> McpServerHandshake {
    McpServerHandshake {
        protocol_version: "2025-11-25".into(),
        server_name: Some("fixture".into()),
        server_version: Some("1".into()),
        capabilities: json!({"tools": {}}),
        instructions: instructions.map(str::to_owned),
    }
}

// ----- direct declarations ----------------------------------------------

#[test]
fn only_ready_direct_tools_are_declared_sorted_with_guaranteed_schemas() {
    let manager = manager();
    manager.insert_connection(
        spec("fs", McpExposure::Direct, &[]),
        Fake::new(),
        vec![
            tool("write", "Writes a file", json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})),
            tool("read", "Reads a file", json!({})),
        ],
    );
    manager.insert_connection(
        spec("web", McpExposure::Gateway, &[]),
        Fake::new(),
        vec![plain_tool("fetch")],
    );
    // Direct, but never connected: nothing to declare yet.
    manager.upsert(spec("ghost", McpExposure::Direct, &[]));
    let definitions = manager.direct_tool_definitions();
    assert_eq!(
        definitions
            .iter()
            .map(|definition| definition["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["mcp__fs__read", "mcp__fs__write"]
    );
    assert_eq!(
        definitions[0],
        json!({
            "name": "mcp__fs__read",
            "description": "Reads a file",
            "input_schema": {"type": "object", "properties": {}},
        })
    );
    assert_eq!(
        definitions[1]["input_schema"],
        json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
    );
}

#[test]
fn tool_exposure_overrides_apply_exact_then_glob_then_server_default() {
    let manager = manager();
    manager.insert_connection(
        spec(
            "gh",
            McpExposure::Gateway,
            &[
                ("get_*", McpExposure::Direct),
                ("get_secret", McpExposure::Hidden),
                ("list_issues", McpExposure::Direct),
            ],
        ),
        Fake::new(),
        vec![
            plain_tool("get_issue"),
            plain_tool("get_secret"),
            plain_tool("list_issues"),
            plain_tool("list_prs"),
        ],
    );
    assert_eq!(
        names(&manager),
        ["mcp__gh__get_issue", "mcp__gh__list_issues"]
    );
    // And the other way round: a direct server hides single tools.
    let manager = self::manager();
    manager.insert_connection(
        spec(
            "gh",
            McpExposure::Direct,
            &[
                ("admin_*", McpExposure::Hidden),
                ("misc", McpExposure::Gateway),
            ],
        ),
        Fake::new(),
        vec![
            plain_tool("admin_drop"),
            plain_tool("misc"),
            plain_tool("read"),
        ],
    );
    assert_eq!(names(&manager), ["mcp__gh__read"]);
}

#[test]
fn disabled_untrusted_and_failed_servers_declare_nothing() {
    let manager = manager();
    let mut disabled = spec("off", McpExposure::Direct, &[]);
    disabled.enabled = false;
    manager.insert_connection(disabled, Fake::new(), vec![plain_tool("t")]);
    let mut blocked = spec("blocked", McpExposure::Direct, &[]);
    blocked.options.block = Some(McpServerBlock::Untrusted);
    manager.insert_connection(blocked, Fake::new(), vec![plain_tool("t")]);
    assert!(names(&manager).is_empty());
}

#[test]
fn colliding_and_overlong_names_get_a_stable_hash_suffix() {
    let manager = manager();
    manager.insert_connection(
        spec("s", McpExposure::Direct, &[]),
        Fake::new(),
        vec![
            plain_tool("a-b"),
            plain_tool("a_b"),
            plain_tool(&"long".repeat(30)),
            plain_tool("plain"),
        ],
    );
    let first = names(&manager);
    assert_eq!(first.len(), 4);
    assert!(first.contains(&"mcp__s__plain".to_owned()));
    let suffixed = first
        .iter()
        .filter(|name| name.starts_with("mcp__s__a_b_"))
        .count();
    assert_eq!(suffixed, 2, "{first:?}");
    assert!(first
        .iter()
        .all(|name| name.len() <= MAX_PROVIDER_TOOL_NAME));
    assert!(first
        .iter()
        .all(|name| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')));
    // Same names on the next computation, even after the set changed.
    manager.insert_connection(
        spec("s", McpExposure::Direct, &[]),
        Fake::new(),
        vec![plain_tool("a_b"), plain_tool("plain")],
    );
    let again = names(&manager);
    assert!(again.contains(&"mcp__s__plain".to_owned()));
    // The remaining a_b keeps a name, resolvable to its owner.
    let a_b = again
        .iter()
        .find(|name| name.starts_with("mcp__s__a_b"))
        .unwrap();
    assert_eq!(
        manager.resolve_direct_tool(a_b),
        Some(("s".to_owned(), "a_b".to_owned()))
    );
}

#[test]
fn definitions_are_cached_until_the_manager_changes() {
    let manager = manager();
    manager.insert_connection(
        spec("fs", McpExposure::Direct, &[]),
        Fake::new(),
        vec![plain_tool("read")],
    );
    let first = manager.direct_tools();
    assert!(Arc::ptr_eq(&first, &manager.direct_tools()));
    manager.insert_connection(
        spec("fs", McpExposure::Direct, &[]),
        Fake::new(),
        vec![plain_tool("read"), plain_tool("write")],
    );
    let second = manager.direct_tools();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(second.len(), 2);
    // `read` kept its name across the change.
    assert_eq!(first[0].name, second[0].name);
}

#[test]
fn declarations_are_capped_and_the_rest_stays_on_the_gateway() {
    let manager = manager();
    let tools: Vec<McpToolSummary> = (0..MAX_DIRECT_TOOLS + 20)
        .map(|index| plain_tool(&format!("t{index:03}")))
        .collect();
    manager.insert_connection(spec("big", McpExposure::Direct, &[]), Fake::new(), tools);
    let declared = names(&manager);
    assert_eq!(declared.len(), MAX_DIRECT_TOOLS);
    assert!(manager.resolve_direct_tool("mcp__big__t000").is_some());
    assert!(manager
        .resolve_direct_tool(&format!("mcp__big__t{:03}", MAX_DIRECT_TOOLS + 5))
        .is_none());
}

#[tokio::test]
async fn a_direct_name_resolves_and_calls_through_the_manager_path() {
    let manager = manager();
    let fake = Fake::new();
    manager.insert_connection(
        spec("fs", McpExposure::Direct, &[]),
        fake.clone(),
        vec![plain_tool("read")],
    );
    let _ = manager.direct_tool_definitions();
    let (server, tool) = manager.resolve_direct_tool("mcp__fs__read").unwrap();
    let value = manager
        .call_with_progress(
            &server,
            &tool,
            json!({"p": 1}),
            McpCancellation::new(),
            None,
        )
        .await
        .into_result()
        .unwrap();
    assert_eq!(value["content"][0]["text"], "pong:read");
    let calls = fake.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["name"], "read");
    assert_eq!(calls[0]["arguments"], json!({"p": 1}));
    // Unknown, foreign or no-longer-direct names do not resolve.
    assert!(manager.resolve_direct_tool("mcp__fs__nope").is_none());
    assert!(manager.resolve_direct_tool("read").is_none());
    manager.upsert(spec("fs", McpExposure::Gateway, &[]));
    assert!(manager.resolve_direct_tool("mcp__fs__read").is_none());
}

// ----- hidden --------------------------------------------------------------

fn hidden_fixture() -> (Arc<McpManager>, Arc<Fake>) {
    let manager = manager();
    let fake = Fake::new();
    manager.insert_connection(
        spec(
            "mix",
            McpExposure::Gateway,
            &[("secret_*", McpExposure::Hidden)],
        ),
        fake.clone(),
        vec![
            tool("public_tool", "Visible tool", json!({"type": "object"})),
            tool(
                "secret_tool",
                "Hidden tool for rockets",
                json!({"type": "object"}),
            ),
        ],
    );
    (manager, fake)
}

#[tokio::test]
async fn hidden_tools_cannot_be_called_by_any_caller() {
    let (manager, fake) = hidden_fixture();
    let error = manager
        .call_with_progress(
            "mix",
            "secret_tool",
            json!({}),
            McpCancellation::new(),
            None,
        )
        .await
        .into_result()
        .unwrap_err();
    assert!(matches!(error, McpError::Blocked(_)), "{error}");
    assert!(error.to_string().contains("hidden by configuration"));
    assert!(
        fake.tool_calls().is_empty(),
        "a hidden tool reached the server"
    );
    // The visible tool works.
    manager.call("mix", "public_tool", json!({})).await.unwrap();
    assert_eq!(fake.tool_calls().len(), 1);
}

#[tokio::test]
async fn hidden_tools_are_absent_from_listings_search_and_describe() {
    let (manager, _fake) = hidden_fixture();
    let listed = manager.list_tools_text("mix", 0).await.unwrap();
    assert!(listed.contains("public_tool"));
    assert!(!listed.contains("secret_tool"), "{listed}");
    let searched = manager
        .search_tools_cancellable(Some("mix"), "rockets", 0, McpCancellation::new())
        .await
        .into_result()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&searched).unwrap()["tools"],
        json!([])
    );
    let global = manager
        .search_tools_cancellable(None, "rockets", 0, McpCancellation::new())
        .await
        .into_result()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&global).unwrap()["tools"],
        json!([])
    );
    let error = manager.describe("mix", "secret_tool").await.unwrap_err();
    assert!(error.to_string().contains("hidden by configuration"));
    assert!(manager.describe("mix", "public_tool").await.is_ok());
    // Codemode helpers read the same filter.
    let hits = manager
        .search_tools_value(None, "tool", 8, McpCancellation::new())
        .await
        .into_result()
        .unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["name"], "mcp.mix.public_tool");
    assert!(manager
        .describe_tool_value("mcp.mix.secret_tool", McpCancellation::new())
        .await
        .into_result()
        .is_err());
    assert_eq!(
        manager
            .describe_tool_value("mcp.mix.public_tool", McpCancellation::new())
            .await
            .into_result()
            .unwrap()["tool"],
        "public_tool"
    );
}

#[tokio::test]
async fn a_fully_hidden_server_is_unreachable_and_never_connected() {
    let manager = manager();
    let fake = Fake::new();
    manager.insert_connection(
        spec("vault", McpExposure::Hidden, &[]),
        fake.clone(),
        vec![plain_tool("open")],
    );
    manager.insert_connection(
        spec("open", McpExposure::Gateway, &[]),
        Fake::new(),
        vec![plain_tool("ok")],
    );
    let listing = manager.list_servers();
    assert!(!listing.contains("vault"), "{listing}");
    assert!(listing.contains("open"));
    assert!(manager.awareness_block().unwrap().contains("- open (ready"));
    assert!(!manager.awareness_block().unwrap().contains("vault"));
    assert!(manager.list_tools_text("vault", 0).await.is_err());
    assert!(manager
        .search_tools_cancellable(Some("vault"), "open", 0, McpCancellation::new())
        .await
        .into_result()
        .is_err());
    assert!(manager.call("vault", "open", json!({})).await.is_err());
    assert!(manager
        .describe_tool_value("mcp.vault.open", McpCancellation::new())
        .await
        .into_result()
        .is_err());
    let servers = manager.list_servers_value();
    assert_eq!(servers.as_array().unwrap().len(), 1);
    assert_eq!(servers[0]["name"], "open");
    assert!(fake.tool_calls().is_empty());
    // Global search ignores it as well.
    let global = manager
        .search_tools_cancellable(None, "open", 0, McpCancellation::new())
        .await
        .into_result()
        .unwrap();
    assert!(!global.contains("mcp.vault"), "{global}");
}

#[test]
fn an_all_hidden_configuration_reports_no_available_servers() {
    let manager = manager();
    manager.insert_connection(
        spec("vault", McpExposure::Hidden, &[]),
        Fake::new(),
        vec![plain_tool("x")],
    );
    assert!(manager.list_servers().contains("hidden"));
    assert!(manager.awareness_block().is_none());
}

#[tokio::test]
async fn a_hidden_override_on_a_direct_server_keeps_the_tool_out_of_the_declarations() {
    let manager = manager();
    let fake = Fake::new();
    manager.insert_connection(
        spec("fs", McpExposure::Direct, &[("rm", McpExposure::Hidden)]),
        fake.clone(),
        vec![plain_tool("read"), plain_tool("rm")],
    );
    assert_eq!(names(&manager), ["mcp__fs__read"]);
    assert!(manager.resolve_direct_tool("mcp__fs__rm").is_none());
    assert!(manager.call("fs", "rm", json!({})).await.is_err());
    assert!(fake.tool_calls().is_empty());
}

// ----- search ----------------------------------------------------------------

#[tokio::test]
async fn gateway_search_ranks_with_bm25_over_name_description_schema_and_server_text() {
    let manager = manager();
    manager.insert_connection(
        spec("github", McpExposure::Gateway, &[]),
        Fake::new(),
        vec![
            tool("list_issues", "List issues of a repository", json!({"type": "object", "properties": {"state": {"description": "open or closed"}}})),
            tool("create_issue", "Create a new issue", json!({"type": "object"})),
            tool("get_weather", "Weather forecast", json!({"type": "object", "properties": {"city": {"description": "City name"}}})),
        ],
    );
    manager.insert_connection(
        spec("maps", McpExposure::Gateway, &[]),
        Fake::new(),
        vec![tool("route", "Plan a route", json!({"type": "object"}))],
    );
    let search = |query: &'static str| {
        let manager = manager.clone();
        async move {
            let text = manager
                .search_tools_cancellable(None, query, 0, McpCancellation::new())
                .await
                .into_result()
                .unwrap();
            serde_json::from_str::<Value>(&text).unwrap()
        }
    };
    let result = search("create issue").await;
    let order: Vec<&str> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(order[0], "mcp.github.create_issue");
    assert_eq!(order[1], "mcp.github.list_issues");
    assert_eq!(result["next_offset"], Value::Null);
    // Plural / singular and schema property descriptions match.
    let result = search("issues").await;
    assert_eq!(result["tools"].as_array().unwrap().len(), 2);
    let result = search("closed").await;
    assert_eq!(result["tools"][0]["name"], "mcp.github.list_issues");
    let result = search("city").await;
    assert_eq!(result["tools"][0]["name"], "mcp.github.get_weather");
    // The server name is part of the text.
    let result = search("maps").await;
    assert_eq!(result["tools"][0]["name"], "mcp.maps.route");
    // A query without any match is empty, not an error.
    assert_eq!(search("zzzzzz").await["tools"], json!([]));
}

#[tokio::test]
async fn search_pages_with_offset_and_names_servers_it_could_not_search() {
    let manager = manager();
    let tools: Vec<McpToolSummary> = (0..40)
        .map(|i| {
            tool(
                &format!("tool_{i:02}"),
                "common words everywhere",
                json!({"type": "object"}),
            )
        })
        .collect();
    manager.insert_connection(spec("big", McpExposure::Gateway, &[]), Fake::new(), tools);
    manager.upsert(spec("later", McpExposure::Gateway, &[]));
    let first: Value = serde_json::from_str(
        &manager
            .search_tools_cancellable(None, "common words", 0, McpCancellation::new())
            .await
            .into_result()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first["tools"].as_array().unwrap().len(), 32);
    assert_eq!(first["next_offset"], 32);
    assert_eq!(first["unsearched_servers"], json!(["later"]));
    let second: Value = serde_json::from_str(
        &manager
            .search_tools_cancellable(None, "common words", 32, McpCancellation::new())
            .await
            .into_result()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(second["tools"].as_array().unwrap().len(), 8);
    assert_eq!(second["next_offset"], Value::Null);
    let error = manager
        .search_tools_cancellable(None, "   ", 0, McpCancellation::new())
        .await
        .into_result()
        .unwrap_err();
    assert!(error.to_string().contains("query"));
}

#[tokio::test]
async fn codemode_search_respects_limit_server_filter_and_description_length() {
    let manager = manager();
    manager.insert_connection(
        spec("a", McpExposure::Gateway, &[]),
        Fake::new(),
        (0..12)
            .map(|i| {
                tool(
                    &format!("item_{i}"),
                    &"é".repeat(400),
                    json!({"type": "object"}),
                )
            })
            .collect(),
    );
    manager.insert_connection(
        spec("b", McpExposure::Gateway, &[]),
        Fake::new(),
        vec![plain_tool("item_b")],
    );
    let hits = manager
        .search_tools_value(None, "item", 5, McpCancellation::new())
        .await
        .into_result()
        .unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 5);
    assert!(hits[0]["description"].as_str().unwrap().chars().count() <= 240);
    let only_b = manager
        .search_tools_value(Some("b"), "item", 5, McpCancellation::new())
        .await
        .into_result()
        .unwrap();
    assert_eq!(only_b.as_array().unwrap().len(), 1);
    assert_eq!(only_b[0]["server"], "b");
    assert_eq!(only_b[0]["tool"], "item_b");
    assert!(manager
        .search_tools_value(Some("nope"), "item", 5, McpCancellation::new())
        .await
        .into_result()
        .is_err());
}

#[tokio::test]
async fn describe_tool_returns_schemas_or_null() {
    let manager = manager();
    let mut with_output = tool(
        "rows",
        "Rows",
        json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
    );
    with_output.output_schema = Some(json!({"type": "object"}));
    manager.insert_connection(
        spec("db", McpExposure::Gateway, &[]),
        Fake::new(),
        vec![with_output],
    );
    let described = manager
        .describe_tool_value("mcp.db.rows", McpCancellation::new())
        .await
        .into_result()
        .unwrap();
    assert_eq!(described["name"], "mcp.db.rows");
    assert_eq!(
        described["inputSchema"]["properties"]["n"]["type"],
        "integer"
    );
    assert_eq!(described["outputSchema"], json!({"type": "object"}));
    assert_eq!(
        manager
            .describe_tool_value("mcp.db.missing", McpCancellation::new())
            .await
            .into_result()
            .unwrap(),
        Value::Null
    );
    for bad in ["db.rows", "mcp.db", "mcp..x", "rows"] {
        assert!(manager
            .describe_tool_value(bad, McpCancellation::new())
            .await
            .into_result()
            .is_err());
    }
}

// ----- listings and awareness -----------------------------------------------

#[tokio::test]
async fn listings_show_description_and_instructions() {
    let manager = manager();
    let mut described = spec("docs", McpExposure::Gateway, &[]);
    described.options.description = Some("Company wiki\nsecond line".into());
    manager.insert_connection_with_handshake(
        described,
        Fake::new(),
        vec![plain_tool("search")],
        handshake(Some("Always search before reading.\nSecond paragraph.")),
    );
    manager.insert_connection(
        spec("plain", McpExposure::Gateway, &[]),
        Fake::new(),
        vec![plain_tool("t")],
    );

    let listing = manager.list_servers();
    assert!(listing.contains("docs [stdio] ready, 1 tools"), "{listing}");
    assert!(
        listing.contains("description: Company wiki second line"),
        "{listing}"
    );
    assert!(listing.contains("instructions (server-provided, untrusted): Always search before reading. Second paragraph."), "{listing}");
    assert!(listing.contains("plain [stdio] ready, 1 tools"));

    let detail = manager.list_tools_text("docs", 0).await.unwrap();
    assert!(detail.starts_with("Server docs [stdio]"), "{detail}");
    assert!(
        detail.contains("description: Company wiki\nsecond line"),
        "{detail}"
    );
    assert!(
        detail.contains("Always search before reading.\nSecond paragraph."),
        "{detail}"
    );
    assert!(detail.contains("search — Does search"), "{detail}");
    // Later pages carry only tools; servers without text show only tools.
    assert!(!manager
        .list_tools_text("docs", 1)
        .await
        .unwrap()
        .contains("Server docs"));
    assert!(!manager
        .list_tools_text("plain", 0)
        .await
        .unwrap()
        .contains("Server plain"));
}

#[test]
fn awareness_block_lists_enabled_non_hidden_servers_with_status_and_summary() {
    let manager = manager();
    let mut docs = spec("docs", McpExposure::Gateway, &[]);
    docs.options.description = Some("Company wiki and runbooks\nmore".into());
    manager.insert_connection(docs, Fake::new(), vec![plain_tool("a"), plain_tool("b")]);
    manager.insert_connection_with_handshake(
        spec(
            "git",
            McpExposure::Direct,
            &[("secret", McpExposure::Hidden)],
        ),
        Fake::new(),
        vec![plain_tool("log"), plain_tool("secret")],
        handshake(Some("Prefer shallow clones.\nMore text.")),
    );
    manager.upsert(spec("later", McpExposure::Gateway, &[]));
    let mut off = spec("off", McpExposure::Gateway, &[]);
    off.enabled = false;
    manager.upsert(off);
    let mut untrusted = spec("proj", McpExposure::Gateway, &[]);
    untrusted.options.block = Some(McpServerBlock::Untrusted);
    manager.upsert(untrusted);
    manager.upsert(spec("vault", McpExposure::Hidden, &[]));

    let block = manager.awareness_block().unwrap();
    let lines: Vec<&str> = block.lines().collect();
    assert!(
        lines[0].contains("server-provided and untrusted"),
        "{block}"
    );
    assert_eq!(
        lines[1],
        "- docs (ready, 2 tools): Company wiki and runbooks"
    );
    assert_eq!(lines[2], "- git (ready, 1 tool): Prefer shallow clones.");
    assert_eq!(lines[3], "- later (disconnected)");
    assert_eq!(lines.len(), 4, "{block}");
}

#[test]
fn the_server_list_is_bounded_with_a_more_servers_line_on_its_own_row() {
    let manager = manager();
    for index in 0..70 {
        manager.upsert(spec(
            &format!("server{index:02}"),
            McpExposure::Gateway,
            &[],
        ));
    }
    let listing = manager.list_servers();
    let lines: Vec<&str> = listing.lines().collect();
    assert_eq!(lines.len(), 65, "{listing}");
    assert!(lines[63].starts_with("server63 [stdio]"));
    assert_eq!(lines[64], "… 6 more servers");
}

#[test]
fn awareness_block_is_bounded_for_many_servers_and_long_text() {
    let manager = manager();
    for index in 0..120 {
        let mut s = spec(&format!("server{index:03}"), McpExposure::Gateway, &[]);
        s.options.description = Some("x".repeat(400));
        manager.upsert(s);
    }
    let block = manager.awareness_block().unwrap();
    assert!(block.len() <= MAX_AWARENESS_BYTES, "{}", block.len());
    assert!(block.lines().last().unwrap().ends_with(" more servers"));
    assert!(block
        .lines()
        .skip(1)
        .all(|line| line.chars().count() <= 250));
}

// ----- end to end through the agent loop --------------------------------------

fn provider(
    calls: Vec<(&'static str, Value)>,
) -> (
    HttpProviderClient<OpenAiCompatibleAdapter>,
    thread::JoinHandle<Vec<Value>>,
) {
    let (listener, address) = bind_listener();
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..=calls.len() {
            let mut stream = accept_within(&listener, Duration::from_secs(8));
            let request = read_http_request(&mut stream);
            requests.push(serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap());
            if let Some((name, arguments)) = calls.get(index) {
                let chunk = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("call-{index}"),"function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]});
                write_sse(&mut stream, &format!("data: {chunk}\n\ndata: [DONE]\n\n"));
            } else {
                write_sse(&mut stream, SSE_FINAL_ANSWER);
            }
        }
        requests
    });
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        format!("http://{address}"),
        "fixture-model",
        "fixture-key",
    ))
    .unwrap();
    (
        HttpProviderClient::new(adapter, Duration::from_secs(5)).unwrap(),
        worker,
    )
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect()
}

fn e2e_manager() -> (Arc<McpManager>, Arc<Fake>) {
    let manager = manager();
    let fake = Fake::new();
    let mut fixture = spec(
        "fixture",
        McpExposure::Direct,
        &[("secret_op", McpExposure::Hidden)],
    );
    fixture.options.description = Some("Fixture server".into());
    manager.insert_connection(
        fixture,
        fake.clone(),
        vec![
            tool(
                "ping",
                "Ping the fixture",
                json!({"type": "object", "properties": {"x": {"type": "integer"}}}),
            ),
            plain_tool("secret_op"),
        ],
    );
    manager.insert_connection(
        spec("docs", McpExposure::Gateway, &[]),
        Fake::new(),
        vec![plain_tool("search")],
    );
    (manager, fake)
}

#[test]
fn a_direct_tool_is_declared_called_and_answered_through_the_agent_loop() {
    let root = TempRoot::new("mcp-exposure-e2e");
    let (manager, fake) = e2e_manager();
    let (client, requests) = provider(vec![
        ("mcp__fixture__ping", json!({"x": 1})),
        (
            "mcp",
            json!({"server": "fixture", "tool": "secret_op", "arguments": {}}),
        ),
        ("mcp__fixture__secret_op", json!({})),
    ]);
    let mut runtime = Runtime::new();
    runtime.set_mcp_manager(Some(manager));
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "use the fixture",
            OperatingMode::Auto,
            root.path(),
            0,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    assert_eq!(result.stop, AgentLoopStop::ProviderCompleted);
    assert_eq!(result.tool_results.len(), 3);

    // 1: executed through the manager as a plain tools/call.
    assert!(
        result.tool_results[0].success,
        "{}",
        result.tool_results[0].output
    );
    assert_eq!(result.tool_results[0].name, "mcp__fixture__ping");
    assert_eq!(result.tool_results[0].output, "pong:ping");
    let calls = fake.tool_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["name"], "ping");
    assert_eq!(calls[0]["arguments"], json!({"x": 1}));
    // 2: hidden through the gateway.
    assert!(!result.tool_results[1].success);
    assert!(
        result.tool_results[1]
            .output
            .contains("hidden by configuration"),
        "{}",
        result.tool_results[1].output
    );
    // 3: a hidden tool has no direct name.
    assert!(!result.tool_results[2].success);
    assert!(
        result.tool_results[2].output.contains("unknown MCP tool"),
        "{}",
        result.tool_results[2].output
    );
    assert_eq!(fake.tool_calls().len(), 1);

    let requests = requests.join().unwrap();
    let names = tool_names(&requests[0]);
    assert!(
        names.contains(&"mcp__fixture__ping".to_owned()),
        "{names:?}"
    );
    assert!(!names.contains(&"mcp__fixture__secret_op".to_owned()));
    assert!(names.contains(&"mcp".to_owned()) && names.contains(&"codemode".to_owned()));
    let declared = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "mcp__fixture__ping")
        .unwrap();
    assert_eq!(declared["function"]["description"], "Ping the fixture");
    assert_eq!(
        declared["function"]["parameters"],
        json!({"type": "object", "properties": {"x": {"type": "integer"}}})
    );
    // Native tools keep their order; direct tools come after them.
    assert!(
        names.iter().position(|n| n == "mcp").unwrap()
            < names
                .iter()
                .position(|n| n == "mcp__fixture__ping")
                .unwrap()
    );
    // The awareness block rides on the user message, after the stanza.
    let user = requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let stanza = user.find("Harness channel: Auto").expect("stanza");
    let block = user.find("MCP servers (").expect("awareness block");
    assert!(stanza < block, "{user}");
    assert!(user.contains("- docs (ready, 1 tool)"), "{user}");
    assert!(
        user.contains("- fixture (ready, 1 tool): Fixture server"),
        "{user}"
    );
    // The tool message carries the direct tool's name.
    let second = requests[1]["messages"].to_string();
    assert!(second.contains("pong:ping"));
}

#[test]
fn plan_and_read_only_never_declare_direct_tools_or_the_awareness_block() {
    let (manager, _fake) = e2e_manager();
    let mut runtime = Runtime::new();
    runtime.set_mcp_manager(Some(manager));
    let auto = runtime.advertised_tool_definitions(OperatingMode::Auto);
    assert!(auto.iter().any(|d| d["name"] == "mcp__fixture__ping"));
    for mode in [OperatingMode::Plan, OperatingMode::ReadOnly] {
        let definitions = runtime.advertised_tool_definitions(mode);
        assert!(
            definitions
                .iter()
                .all(|d| !d["name"].as_str().unwrap().starts_with("mcp")),
            "{mode:?}"
        );
    }
    // The gateway definition is the same with and without direct tools.
    let gateway = |definitions: &[Value]| {
        definitions
            .iter()
            .find(|definition| definition["name"] == "mcp")
            .cloned()
            .unwrap()
    };
    let mut gateway_only = Runtime::new();
    let manager = Arc::new(McpManager::new(
        BTreeMap::new(),
        PathBuf::from("."),
        ExecutableResolver::default(),
    ));
    manager.insert_connection(
        spec("docs", McpExposure::Gateway, &[]),
        Fake::new(),
        vec![plain_tool("search")],
    );
    gateway_only.set_mcp_manager(Some(manager));
    assert_eq!(
        gateway(&auto),
        gateway(&gateway_only.advertised_tool_definitions(OperatingMode::Auto))
    );
    assert!(gateway_only
        .advertised_tool_definitions(OperatingMode::Auto)
        .iter()
        .all(|d| !d["name"].as_str().unwrap().starts_with("mcp__")));
}

#[test]
fn a_run_without_mcp_has_no_awareness_block() {
    let root = TempRoot::new("mcp-exposure-none");
    let (client, requests) = provider(vec![]);
    let mut runtime = Runtime::new();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "hello",
            OperatingMode::Auto,
            root.path(),
            0,
            AgentLoopConfig::default(),
        ))
        .unwrap();
    let requests = requests.join().unwrap();
    assert!(!requests[0]["messages"]
        .to_string()
        .contains("MCP servers ("));
}

// ----- the manager path: gate, redaction, progress, budgets, cancellation ------

/// Answers `tools/call` with a fixed text and reports progress first.
struct Scripted {
    text: String,
    progress: Vec<McpProgress>,
    calls: Mutex<Vec<Value>>,
}

impl Scripted {
    fn new(text: &str, progress: Vec<McpProgress>) -> Arc<Self> {
        Arc::new(Self {
            text: text.to_owned(),
            progress,
            calls: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl McpConnection for Scripted {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        assert_eq!(method, "tools/call");
        self.calls.lock().unwrap().push(params);
        Ok(json!({"content": [{"type": "text", "text": self.text}]}))
    }

    async fn request_with_progress(
        &self,
        method: &str,
        params: Value,
        _cancellation: McpCancellation,
        progress: Option<McpProgressSink>,
    ) -> McpRequestOutcome<Value> {
        if let Some(sink) = progress {
            for update in &self.progress {
                sink(update.clone());
            }
        }
        McpRequestOutcome::Completed(self.request(method, params).await)
    }

    async fn notify(&self, _method: &str, _params: Value) {}

    fn is_closed(&self) -> bool {
        false
    }
}

fn run_direct(
    runtime: &mut Runtime,
    root: &std::path::Path,
    calls: Vec<(&'static str, Value)>,
) -> Result<slim_core::AgentLoopResult, slim_core::provider::ProviderError> {
    let (client, requests) = provider(calls);
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(runtime.run_agent_loop(
            &client,
            "use the fixture",
            OperatingMode::Auto,
            root,
            0,
            AgentLoopConfig::default(),
        ));
    drop(client);
    // The fixture may still wait for requests the loop never sent.
    if result.is_ok() {
        let _ = requests.join();
    }
    result
}

fn direct_manager(connection: Arc<dyn McpConnection>) -> Arc<McpManager> {
    let manager = manager();
    manager.insert_connection(
        spec("fixture", McpExposure::Direct, &[]),
        connection,
        vec![plain_tool("ping")],
    );
    manager
}

#[test]
fn direct_arguments_with_registered_secrets_never_reach_the_server() {
    let root = TempRoot::new("mcp-exposure-secret");
    let connection = Scripted::new("pong", vec![]);
    let mut runtime = Runtime::new();
    runtime.set_mcp_manager(Some(direct_manager(connection.clone())));
    runtime.register_sensitive_value("tok-123456789");
    let result = run_direct(
        &mut runtime,
        root.path(),
        vec![(
            "mcp__fixture__ping",
            json!({"auth": "Bearer tok-123456789"}),
        )],
    );
    let error = result.expect_err("a tool call carrying a secret is refused");
    assert!(
        format!("{error:?}").contains("registered sensitive material"),
        "{error:?}"
    );
    assert!(connection.calls.lock().unwrap().is_empty());
}

#[test]
fn direct_results_are_redacted_and_bounded() {
    let root = TempRoot::new("mcp-exposure-redact");
    let huge = format!("tok-123456789 {}", "x".repeat(200 * 1024));
    let connection = Scripted::new(&huge, vec![]);
    let mut runtime = Runtime::new();
    runtime.set_mcp_manager(Some(direct_manager(connection)));
    runtime.register_sensitive_value("tok-123456789");
    let result = run_direct(
        &mut runtime,
        root.path(),
        vec![("mcp__fixture__ping", json!({}))],
    )
    .unwrap();
    let output = &result.tool_results[0].output;
    assert!(result.tool_results[0].success);
    assert!(!output.contains("tok-123456789"), "secret leaked");
    assert!(output.len() < 80 * 1024, "{} bytes", output.len());
    assert!(!runtime
        .app
        .events()
        .iter()
        .any(|event| format!("{:?}", event.kind).contains("tok-123456789")));
}

#[test]
fn direct_calls_surface_server_progress_as_tool_progress_events() {
    let root = TempRoot::new("mcp-exposure-progress");
    let connection = Scripted::new(
        "done",
        vec![McpProgress {
            progress: 1.0,
            total: Some(2.0),
            message: Some("half way tok-123456789".into()),
        }],
    );
    let mut runtime = Runtime::new();
    runtime.set_mcp_manager(Some(direct_manager(connection)));
    runtime.register_sensitive_value("tok-123456789");
    let result = run_direct(
        &mut runtime,
        root.path(),
        vec![("mcp__fixture__ping", json!({}))],
    )
    .unwrap();
    assert!(result.tool_results[0].success);
    let progress: Vec<String> = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolProgress { name, preview, .. } if name == "mcp__fixture__ping" => {
                Some(preview.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(progress.len(), 1, "{progress:?}");
    assert!(
        progress[0].contains("progress 1/2 (50%)"),
        "{}",
        progress[0]
    );
    assert!(progress[0].contains("half way"));
    assert!(!progress[0].contains("tok-123456789"));
}

struct Hanging {
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    calls: std::sync::atomic::AtomicUsize,
    closed: AtomicBool,
}

#[async_trait::async_trait]
impl McpConnection for Hanging {
    async fn request(&self, method: &str, _params: Value) -> Result<Value, McpError> {
        Err(McpError::Protocol(format!(
            "unexpected non-cancellable request {method}"
        )))
    }

    async fn request_cancellable(
        &self,
        _method: &str,
        _params: Value,
        cancellation: McpCancellation,
    ) -> McpRequestOutcome<Value> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(started) = self.started.lock().unwrap().take() {
            let _ = started.send(());
        }
        cancellation.cancelled().await;
        McpRequestOutcome::OutcomeUncertain {
            interruption: McpInterruption::Cancelled,
            cleanup: McpCleanupStatus::Confirmed,
        }
    }

    async fn close_for_cleanup(&self) -> McpCleanupStatus {
        self.closed.store(true, Ordering::Release);
        McpCleanupStatus::Confirmed
    }

    async fn notify(&self, _method: &str, _params: Value) {}

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

#[test]
fn cancelling_a_direct_call_reports_an_uncertain_outcome_and_never_replays() {
    let root = TempRoot::new("mcp-exposure-cancel");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let connection = Arc::new(Hanging {
        started: Mutex::new(Some(started_tx)),
        calls: std::sync::atomic::AtomicUsize::new(0),
        closed: AtomicBool::new(false),
    });
    let token = CancellationToken::new();
    let mut runtime = Runtime::new();
    runtime.set_cancellation_token(token.clone());
    runtime.set_mcp_manager(Some(direct_manager(connection.clone())));
    let (client, requests) = provider(vec![("mcp__fixture__ping", json!({"op": "write"}))]);
    let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let run = runtime.run_agent_loop(
            &client,
            "write once",
            OperatingMode::Auto,
            root.path(),
            0,
            AgentLoopConfig {
                max_turns: 1,
                ..AgentLoopConfig::default()
            },
        );
        let cancel_when_called = async {
            tokio::time::timeout(Duration::from_secs(3), started_rx)
                .await
                .expect("the call starts")
                .expect("start signal");
            token.cancel();
        };
        let (result, ()) = tokio::join!(run, cancel_when_called);
        result
    });
    drop(client);
    drop(requests);
    let result = result.expect("cooperative cancellation is a loop result");
    assert_eq!(result.stop, AgentLoopStop::Cancelled);
    assert_eq!(connection.calls.load(Ordering::Relaxed), 1);
    assert!(connection.is_closed());
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(result.tool_results[0].name, "mcp__fixture__ping");
    assert!(!result.tool_results[0].success);
    assert!(result.tool_results[0]
        .output
        .contains("outcome is uncertain"));
    assert!(result.tool_results[0].output.contains("do not replay"));
}

#[tokio::test]
async fn direct_tools_stay_declared_after_a_cancelled_call_resets_the_server() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let connection = Arc::new(Hanging {
        started: Mutex::new(Some(started_tx)),
        calls: std::sync::atomic::AtomicUsize::new(0),
        closed: AtomicBool::new(false),
    });
    let manager = direct_manager(connection.clone());
    let declared = names(&manager);
    assert_eq!(declared, ["mcp__fixture__ping"]);
    let cancellation = McpCancellation::new();
    let call = manager.call_cancellable("fixture", "ping", json!({}), cancellation.clone());
    let cancel = async {
        tokio::time::timeout(Duration::from_secs(3), started_rx)
            .await
            .expect("the call starts")
            .expect("start signal");
        cancellation.cancel();
    };
    let (outcome, ()) = tokio::join!(call, cancel);
    assert!(matches!(
        outcome,
        McpRequestOutcome::OutcomeUncertain { .. }
    ));
    // The cancellation dropped the connection...
    assert!(matches!(
        manager.statuses()[0].status,
        slim_core::mcp::McpServerStatus::Disconnected
    ));
    // ...but the request still carries the same tools, in the same order.
    assert_eq!(names(&manager), declared);
}

// ----- real transport: background connect, first-request wait, direct call ------

mod http_backed {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::mock::{serve, Mock, Script};
    use serde_json::json;
    use slim_core::mcp::{
        McpCancellation, McpExposure, McpManager, McpServerSpec, McpServerStatus, McpTransport,
    };
    use slim_core::process::ExecutableResolver;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    fn slow_server(delay: Duration) -> Mock {
        let mut script = Script::new();
        script.init = Box::new(move |_, _, _| {
            thread::sleep(delay);
            true
        });
        serve(script)
    }

    fn http_spec(name: &str, mock: &Mock, exposure: McpExposure) -> McpServerSpec {
        let mut spec = McpServerSpec::new(
            name,
            McpTransport::Http {
                url: mock.url.clone(),
                headers: BTreeMap::new(),
            },
        );
        spec.timeout = Duration::from_secs(10);
        spec.options.exposure = exposure;
        spec
    }

    fn manager_of(specs: Vec<McpServerSpec>) -> Arc<McpManager> {
        Arc::new(McpManager::new(
            specs
                .into_iter()
                .map(|spec| (spec.name.clone(), spec))
                .collect(),
            PathBuf::from("."),
            ExecutableResolver::default(),
        ))
    }

    fn is_ready(manager: &McpManager, name: &str) -> bool {
        manager
            .statuses()
            .into_iter()
            .find(|info| info.name == name)
            .is_some_and(|info| matches!(info.status, McpServerStatus::Ready { .. }))
    }

    #[test]
    fn the_first_request_waits_for_a_server_with_a_direct_tool_override_then_declares_and_calls_it()
    {
        let server = slow_server(Duration::from_millis(400));
        let mut spec = http_spec("mixed", &server, McpExposure::Gateway);
        spec.options
            .tool_exposure
            .insert("echo".into(), McpExposure::Direct);
        let manager = manager_of(vec![spec]);
        let rt = runtime();
        rt.block_on(async { manager.start_background_connect() });
        // Nothing is declared while the server is still connecting.
        assert!(manager.direct_tool_definitions().is_empty());
        let begun = Instant::now();
        let report = rt.block_on(manager.wait_for_direct_servers(Duration::from_secs(5)));
        assert!(report.still_connecting.is_empty(), "{report:?}");
        assert!(begun.elapsed() >= Duration::from_millis(300));
        let definitions = manager.direct_tool_definitions();
        assert_eq!(definitions.len(), 1, "{definitions:?}");
        assert_eq!(definitions[0]["name"], "mcp__mixed__echo");
        // The declared tool runs over the real transport through the manager.
        let (name, tool) = manager.resolve_direct_tool("mcp__mixed__echo").unwrap();
        let value = rt
            .block_on(manager.call_with_progress(
                &name,
                &tool,
                json!({"q": 1}),
                McpCancellation::new(),
                None,
            ))
            .into_result()
            .unwrap();
        assert_eq!(value["content"][0]["text"], "ok");
        let calls: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|request| request.is("POST", "tools/call"))
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].body["params"]["name"], "echo");
        assert!(
            calls[0].body["params"]["_meta"]["progressToken"].is_string()
                || calls[0].body["params"]["_meta"]["progressToken"].is_number(),
            "the call carries a progress token like any gateway call"
        );
        rt.block_on(manager.disconnect_all());
    }

    #[test]
    fn a_server_hidden_except_for_some_tools_connects_but_a_fully_hidden_one_does_not() {
        let partly = serve(Script::new());
        let fully = serve(Script::new());
        let mut partly_spec = http_spec("partly", &partly, McpExposure::Hidden);
        partly_spec
            .options
            .tool_exposure
            .insert("echo".into(), McpExposure::Gateway);
        let fully_spec = http_spec("fully", &fully, McpExposure::Hidden);
        let manager = manager_of(vec![partly_spec, fully_spec]);
        let rt = runtime();
        assert_eq!(rt.block_on(async { manager.start_background_connect() }), 1);
        let end = Instant::now() + Duration::from_secs(10);
        while !is_ready(&manager, "partly") {
            assert!(Instant::now() < end, "partly hidden server never connected");
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(200));
        assert!(
            fully.requests().is_empty(),
            "a hidden server is never contacted"
        );
        rt.block_on(manager.disconnect_all());
    }
}
