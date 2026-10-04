use super::*;
use crate::mcp::{
    McpConnection, McpError, McpExposure, McpServerSpec, McpToolSummary, McpTransport,
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl McpConnection for Counting {
    async fn request(&self, _method: &str, _params: Value) -> Result<Value, McpError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(json!({"content": [{"type": "text", "text": "called"}]}))
    }

    async fn notify(&self, _method: &str, _params: Value) {}

    fn is_closed(&self) -> bool {
        false
    }
}

fn spec(name: &str, exposure: McpExposure, hidden: &[&str]) -> McpServerSpec {
    let mut spec = McpServerSpec::new(
        name,
        McpTransport::Stdio {
            command: "controlled-test-connection".into(),
            args: Vec::new(),
            env: Default::default(),
        },
    );
    spec.options.exposure = exposure;
    spec.options.description = Some(format!("{name} server"));
    spec.options.tool_exposure = hidden
        .iter()
        .map(|tool| ((*tool).to_owned(), McpExposure::Hidden))
        .collect();
    spec
}

fn tool(name: &str, description: &str) -> McpToolSummary {
    McpToolSummary {
        name: name.into(),
        description: Some(description.into()),
        schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        output_schema: None,
    }
}

fn runtime_with(connection: Arc<Counting>) -> Runtime {
    let manager = Arc::new(McpManager::new(
        Default::default(),
        std::path::PathBuf::from("."),
        Default::default(),
    ));
    manager.insert_connection(
        spec("mix", McpExposure::Gateway, &["secret_tool"]),
        connection,
        vec![
            tool("find_rockets", "Search the rocket catalog"),
            tool("secret_tool", "Hidden rocket tool"),
            tool("weather", "Weather forecast"),
        ],
    );
    manager.insert_connection(
        spec("vault", McpExposure::Hidden, &[]),
        Arc::new(Counting {
            calls: AtomicUsize::new(0),
        }),
        vec![tool("open", "Open the vault")],
    );
    let mut runtime = Runtime::new();
    runtime.set_mcp_manager(Some(manager));
    runtime
}

async fn eval(runtime: &mut Runtime, code: &str) -> Result<String, String> {
    let arguments = json!({"code": code}).to_string();
    let mut seq = runtime.app.events().last().map_or(0, |event| event.seq + 1);
    runtime
        .run_codemode(
            ToolInvocation {
                batch_id: "cell",
                call_id: "test",
                name: "codemode",
                arguments: &arguments,
            },
            &mut seq,
            Duration::from_secs(5),
        )
        .await
        .unwrap()
}

fn json_of(result: Result<String, String>) -> Value {
    serde_json::from_str(&result.expect("cell succeeded")).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_tools_ranks_limits_and_never_returns_hidden_tools() {
    let mut runtime = runtime_with(Arc::new(Counting {
        calls: AtomicUsize::new(0),
    }));
    let hits = json_of(eval(&mut runtime, "return searchTools('rocket');").await);
    let names: Vec<&str> = hits
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["mcp.mix.find_rockets"], "{hits}");
    assert_eq!(hits[0]["server"], "mix");
    assert_eq!(hits[0]["tool"], "find_rockets");
    assert_eq!(hits[0]["description"], "Search the rocket catalog");
    // The server's own text is searchable, the limit is honored and the
    // server filter narrows the corpus.
    let hits = json_of(
        eval(
            &mut runtime,
            "return searchTools('mix server', {limit: 2});",
        )
        .await,
    );
    assert_eq!(hits.as_array().unwrap().len(), 2);
    let hits = json_of(
        eval(
            &mut runtime,
            "return searchTools('weather', {server: 'mix'}).map(h => h.name);",
        )
        .await,
    );
    assert_eq!(hits, json!(["mcp.mix.weather"]));
    // A hidden server is not searchable, even when named.
    let error = eval(
        &mut runtime,
        "return searchTools('vault open', {server: 'vault'});",
    )
    .await
    .unwrap_err();
    assert!(error.contains("hidden by configuration"), "{error}");
    let hits = json_of(eval(&mut runtime, "return searchTools('vault open');").await);
    assert_eq!(hits, json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_tool_and_list_servers_expose_catalogs_without_hidden_entries() {
    let mut runtime = runtime_with(Arc::new(Counting {
        calls: AtomicUsize::new(0),
    }));
    let described = json_of(eval(&mut runtime, "return describeTool('mcp.mix.weather');").await);
    assert_eq!(described["name"], "mcp.mix.weather");
    assert_eq!(
        described["inputSchema"]["properties"]["q"]["type"],
        "string"
    );
    assert_eq!(
        json_of(eval(&mut runtime, "return describeTool('mcp.mix.nothing');").await),
        Value::Null
    );
    for hidden in ["mcp.mix.secret_tool", "mcp.vault.open"] {
        let error = eval(&mut runtime, &format!("return describeTool('{hidden}');"))
            .await
            .unwrap_err();
        assert!(error.contains("hidden by configuration"), "{error}");
    }
    let servers = json_of(eval(&mut runtime, "return listServers();").await);
    let servers = servers.as_array().unwrap();
    assert_eq!(servers.len(), 1, "{servers:?}");
    assert_eq!(servers[0]["name"], "mix");
    assert_eq!(servers[0]["status"], "ready");
    assert_eq!(servers[0]["tools"], 2);
    assert_eq!(servers[0]["description"], "mix server");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helpers_validate_their_arguments() {
    let mut runtime = runtime_with(Arc::new(Counting {
        calls: AtomicUsize::new(0),
    }));
    for (code, expected) in [
        ("searchTools(1)", "expects a query string"),
        ("searchTools('x', {limit: 0})", "positive integer"),
        ("searchTools('x', {limit: 1.5})", "positive integer"),
        ("searchTools('x', {limit: '2'})", "positive integer"),
        ("searchTools('x', {server: 3})", "server must be a string"),
        ("describeTool(5)", "expects a tool name"),
        ("describeTool('rows')", "mcp.server.tool"),
        ("searchTools('   ')", "query"),
    ] {
        let error = eval(&mut runtime, code).await.unwrap_err();
        assert!(error.contains(expected), "{code}: {error}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_does_not_use_the_tool_budget_and_hidden_calls_are_refused() {
    let connection = Arc::new(Counting {
        calls: AtomicUsize::new(0),
    });
    let mut runtime = runtime_with(connection.clone());
    runtime.codemode.remaining_calls = 3;
    json_of(
        eval(
            &mut runtime,
            "searchTools('rocket'); describeTool('mcp.mix.weather'); listServers(); return 1;",
        )
        .await,
    );
    assert_eq!(runtime.codemode.remaining_calls, 3);
    assert_eq!(runtime.codemode.used_calls, 0);
    // A hidden tool cannot be called from a script either: nothing is sent.
    let error = eval(
        &mut runtime,
        "return await tools.call('mcp.mix.secret_tool', {});",
    )
    .await
    .unwrap_err();
    assert!(error.contains("hidden by configuration"), "{error}");
    assert_eq!(connection.calls.load(Ordering::Relaxed), 0);
    // A visible tool still works and consumes the budget.
    assert_eq!(
        json_of(
            eval(
                &mut runtime,
                "const r = await tools.call('mcp.mix.weather', {}); return r.content[0].text;"
            )
            .await
        ),
        json!("called")
    );
    assert_eq!(connection.calls.load(Ordering::Relaxed), 1);
    // The refused attempt counted against the budget like any host call.
    assert_eq!(runtime.codemode.remaining_calls, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_results_are_redacted_and_need_a_manager() {
    let mut runtime = runtime_with(Arc::new(Counting {
        calls: AtomicUsize::new(0),
    }));
    runtime.register_sensitive_value("tok-123456789");
    runtime.mcp.as_ref().unwrap().insert_connection(
        spec("leaky", McpExposure::Gateway, &[]),
        Arc::new(Counting {
            calls: AtomicUsize::new(0),
        }),
        vec![tool("leak", "Uses tok-123456789 as the key")],
    );
    let hits = eval(
        &mut runtime,
        "return searchTools('leak', {server: 'leaky'});",
    )
    .await
    .unwrap();
    assert!(!hits.contains("tok-123456789"), "{hits}");
    assert!(hits.contains("mcp.leaky.leak"));
    let mut bare = Runtime::new();
    for code in ["return searchTools('x');", "return listServers();"] {
        let error = eval(&mut bare, code).await.unwrap_err();
        assert!(error.contains("no MCP servers configured"), "{error}");
    }
}
