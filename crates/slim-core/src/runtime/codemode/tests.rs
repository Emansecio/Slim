use super::*;
use crate::session::{
    DurableSessionHeader, JsonlRepo, ManualRunJournal, ManualRunSpec, ProviderResponse,
};

async fn eval(runtime: &mut Runtime, code: &str) -> Result<String, String> {
    let arguments = json!({"code": code}).to_string();
    let mut seq = runtime.app.events().last().map_or(0, |event| event.seq + 1);
    runtime
        .run_codemode(
            crate::OperatingMode::Auto,
            Path::new("."),
            ToolInvocation {
                batch_id: "cell",
                call_id: "test",
                name: "codemode",
                arguments: &arguments,
            },
            &mut seq,
            Duration::from_secs(2),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn javascript_composes_and_exposes_only_explicit_host_capabilities() {
    let mut runtime = Runtime::new();
    let result = eval(
        &mut runtime,
        r#"
        const numbers = await Promise.all([Promise.resolve(2), Promise.resolve(3)]);
        return {sum: numbers.reduce((a,b) => a+b, 0),
            globals: [typeof process, typeof require, typeof fetch, typeof __host]};
    "#,
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap(),
        json!({
            "sum": 5, "globals": ["undefined", "undefined", "undefined", "undefined"],
        })
    );
    assert!(eval(&mut runtime, "return await import('fs');")
        .await
        .is_err());
}

#[tokio::test]
async fn stored_json_survives_errors_but_globals_do_not_leak_between_cells() {
    let mut runtime = Runtime::new();
    let error = eval(
        &mut runtime,
        "globalThis.ephemeral = 7; store('items', [2,4]); throw new Error('after store');",
    )
    .await
    .unwrap_err();
    assert!(error.contains("after store"), "{error}");
    assert_eq!(
        eval(&mut runtime, "return [load('items'), typeof ephemeral];")
            .await
            .unwrap(),
        "[[2,4],\"undefined\"]"
    );
    let error = eval(&mut runtime, "store('too-large', 'x'.repeat(512 * 1024));")
        .await
        .unwrap_err();
    assert!(error.contains("512 KiB"), "{error}");
    assert_eq!(
        eval(
            &mut runtime,
            "store('items', null); return [load('items'), load('too-large')];"
        )
        .await
        .unwrap(),
        "[null,null]"
    );
}

#[tokio::test]
async fn invalid_json_state_fails_without_mutating_stored_values() {
    let mut runtime = Runtime::new();
    for code in [
        "const a={}; a.a=a; store('x',a);",
        "store('x', 1n);",
        "store('', 1);",
    ] {
        assert!(eval(&mut runtime, code).await.is_err(), "{code}");
        assert!(runtime.codemode.values.is_empty());
    }
}

#[tokio::test]
async fn interpreter_enforces_memory_and_output_bounds() {
    let mut runtime = Runtime::new();
    assert!(eval(&mut runtime, "return 'x'.repeat(1048577);")
        .await
        .unwrap_err()
        .contains("1 MiB"));
    let error = eval(&mut runtime, "return 'x'.repeat(64 * 1024 * 1024);")
        .await
        .unwrap_err();
    assert!(
        error.contains("memory") || error.contains("allocation"),
        "{error}"
    );
    assert_eq!(eval(&mut runtime, "return 42;").await.unwrap(), "42");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_and_dropped_future_stop_the_native_worker() {
    let mut runtime = Runtime::new();
    let token = CancellationToken::new();
    runtime.cancellation = Some(token.clone());
    let arguments = json!({"code": "while (true) {}"}).to_string();
    let call = ToolInvocation {
        batch_id: "cell",
        call_id: "loop",
        name: "codemode",
        arguments: &arguments,
    };
    let started = Instant::now();
    let result = runtime
        .run_codemode(
            crate::OperatingMode::Auto,
            Path::new("."),
            call,
            &mut 0,
            Duration::from_millis(50),
        )
        .await
        .unwrap();
    assert!(result.is_err());
    token.wait_for_native_work().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    let dropped = tokio::time::timeout(
        Duration::from_millis(30),
        runtime.run_codemode(
            crate::OperatingMode::Auto,
            Path::new("."),
            call,
            &mut 0,
            CELL_TIMEOUT,
        ),
    )
    .await;
    assert!(dropped.is_err());
    tokio::time::timeout(Duration::from_secs(2), token.wait_for_native_work())
        .await
        .unwrap();
}

#[tokio::test]
async fn journal_restores_stored_json_without_executing_code() {
    let root = std::env::temp_dir().join(format!(
        "slim-codemode-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("session.jsonl");
    {
        let repo = JsonlRepo::create(
            &path,
            DurableSessionHeader::new("code", "now", root.to_string_lossy(), None, None),
        )
        .unwrap();
        let journal = Arc::new(Mutex::new(
            ManualRunJournal::start(
                repo,
                ManualRunSpec::new("one", "attempt-one", "input-one", "final-one", "store", 0),
            )
            .unwrap(),
        ));
        let mut runtime = Runtime::new();
        runtime.register_sensitive_value("secret-fixture-token");
        runtime.app.set_run_journal(journal.clone());
        eval(
            &mut runtime,
            "store('rows', {count:3, token:'secret-fixture-token', nested:{'secret-fixture-token':true}}); return 3;",
        )
        .await
        .unwrap();
        journal
            .lock()
            .unwrap()
            .finish(ProviderResponse::new("done", None))
            .unwrap();
    }
    let persisted = std::fs::read_to_string(&path).unwrap();
    assert!(!persisted.contains("secret-fixture-token"));
    {
        let repo = JsonlRepo::open_no_repair(&path).unwrap();
        let first_seq = repo.next_seq().unwrap();
        let journal = Arc::new(Mutex::new(
            ManualRunJournal::start(
                repo,
                ManualRunSpec::new(
                    "two",
                    "attempt-two",
                    "input-two",
                    "final-two",
                    "load",
                    first_seq,
                ),
            )
            .unwrap(),
        ));
        let mut runtime = Runtime::new();
        runtime.app.set_run_journal(journal.clone());
        assert_eq!(
            eval(&mut runtime, "return load('rows').count;")
                .await
                .unwrap(),
            "3"
        );
        assert_eq!(runtime.codemode.used_calls, 0);
        journal
            .lock()
            .unwrap()
            .finish(ProviderResponse::new("done", None))
            .unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn readonly_and_plan_reject_code_even_when_provider_hallucinates_the_tool() {
    for mode in [crate::OperatingMode::ReadOnly, crate::OperatingMode::Plan] {
        let mut runtime = Runtime::new();
        let args = json!({"code":"store('should-not-exist', 1);"}).to_string();
        let (result, _) = runtime
            .execute_codemode_with_governor(
                mode,
                Path::new("."),
                ToolInvocation {
                    batch_id: "cell",
                    call_id: "denied",
                    name: "codemode",
                    arguments: &args,
                },
                0,
                None,
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(runtime.codemode.values.is_empty());
    }
}

#[tokio::test]
async fn dynamically_constructed_arguments_pass_the_existing_secret_gate() {
    let mut runtime = Runtime::new();
    runtime.register_sensitive_value("secret-fixture-token");
    runtime.codemode.remaining_calls = 8;
    let error = eval(
        &mut runtime,
        "return await tools.call('mcp.fixture.rows', {['secret-' + 'fixture-token']: 1});",
    )
    .await
    .unwrap_err();
    assert!(error.contains("registered sensitive material"), "{error}");
    assert_eq!(runtime.codemode.used_calls, 0);
}

// ---- Code mode host ----

/// Workspace for a Code-mode cell; removed on drop.
struct Workspace(std::path::PathBuf);

impl Workspace {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "slim-codemode-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn eval_in(
    runtime: &mut Runtime,
    mode: crate::OperatingMode,
    cwd: &Path,
    code: &str,
) -> Result<String, String> {
    let arguments = json!({"code": code}).to_string();
    let mut seq = runtime.app.events().last().map_or(0, |event| event.seq + 1);
    runtime
        .run_codemode(
            mode,
            cwd,
            ToolInvocation {
                batch_id: "cell",
                call_id: "code",
                name: "codemode",
                arguments: &arguments,
            },
            &mut seq,
            Duration::from_secs(10),
        )
        .await
        .unwrap()
}

/// An MCP tool that takes `delay_ms` and logs when each call starts and
/// ends, with how many calls were in flight and whether `marker` existed.
struct Probe {
    marker: std::path::PathBuf,
    in_flight: std::sync::atomic::AtomicUsize,
    log: Mutex<Vec<(String, String, usize, bool)>>,
}

impl Probe {
    fn record(&self, phase: &str, id: &str, in_flight: usize) {
        self.log
            .lock()
            .unwrap()
            .push((phase.into(), id.into(), in_flight, self.marker.exists()));
    }

    fn entry(&self, phase: &str, id: &str) -> (usize, bool) {
        self.log
            .lock()
            .unwrap()
            .iter()
            .find(|entry| entry.0 == phase && entry.1 == id)
            .map(|entry| (entry.2, entry.3))
            .unwrap_or_else(|| panic!("no {phase} of {id}"))
    }
}

#[async_trait::async_trait]
impl crate::mcp::McpConnection for Probe {
    async fn request(&self, _method: &str, params: Value) -> Result<Value, crate::mcp::McpError> {
        use std::sync::atomic::Ordering;
        let id = params["arguments"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.record("start", &id, now);
        let delay = params["arguments"]["delay_ms"].as_u64().unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let now = self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.record("end", &id, now);
        Ok(json!({"content": [], "structuredContent": {"id": id}}))
    }

    async fn notify(&self, _method: &str, _params: Value) {}

    fn is_closed(&self) -> bool {
        false
    }
}

fn probe_runtime(marker: std::path::PathBuf) -> (Runtime, Arc<Probe>) {
    let probe = Arc::new(Probe {
        marker,
        in_flight: Default::default(),
        log: Mutex::new(Vec::new()),
    });
    let manager = Arc::new(McpManager::new(
        Default::default(),
        std::path::PathBuf::from("."),
        Default::default(),
    ));
    manager.insert_connection(
        crate::mcp::McpServerSpec::new(
            "probe",
            crate::mcp::McpTransport::Stdio {
                command: "controlled-test-connection".into(),
                args: Vec::new(),
                env: Default::default(),
            },
        ),
        probe.clone(),
        vec![crate::mcp::McpToolSummary {
            name: "wait".into(),
            description: Some("Wait, then answer".into()),
            schema: json!({"type": "object", "properties": {"id": {"type": "string"}, "delay_ms": {"type": "integer"}}}),
            output_schema: Some(json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]})),
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: false,
        }],
    );
    let mut runtime = Runtime::new();
    runtime.set_mcp_manager(Some(manager));
    runtime.codemode.remaining_calls = 32;
    (runtime, probe)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_mode_runs_reads_and_mcp_in_parallel_and_mutations_alone_in_order() {
    let workspace = Workspace::new("schedule");
    std::fs::write(workspace.0.join("input.txt"), "alpha\n").unwrap();
    let (mut runtime, probe) = probe_runtime(workspace.0.join("out.txt"));
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const wait = (id, delay_ms) => tools.call('mcp.probe.wait', {id, delay_ms});
        const slow = wait('slow', 400);
        const text = await slim.read({path: 'input.txt'});
        // The read did not wait for the slow call still in flight.
        await wait('after-read', 0);
        const [a, b, written, c] = await Promise.all([
            wait('a', 150), wait('b', 150),
            slim.write({path: 'out.txt', content: text}),
            wait('c', 0),
        ]);
        await slow;
        return [a.id, b.id, c.id, written.length > 0];
        "#,
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!(["a", "b", "c", true])
    );
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("out.txt")).unwrap(),
        "alpha\n"
    );
    // `after-read` started while `slow` was in flight: reads run alongside.
    assert_eq!(probe.entry("start", "after-read").0, 2);
    // `a` and `b` ran together, and both before the write.
    assert!(probe.entry("start", "b").0 >= 2 || probe.entry("start", "a").0 >= 2);
    assert!(!probe.entry("end", "a").1 && !probe.entry("end", "b").1);
    // `c` waited for the write, which waited for everything before it.
    assert!(probe.entry("start", "c").1);
    assert!(
        !probe.entry("end", "slow").1,
        "the write waited for the call in flight"
    );
    assert_eq!(runtime.codemode.used_calls, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_works_without_mcp_and_native_failures_reject() {
    let workspace = Workspace::new("natives");
    std::fs::write(workspace.0.join("a.txt"), "needle here\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 8;
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const [listing, hits] = await Promise.all([slim.list(), slim.search({query: 'needle'})]);
        const failures = [];
        for (const call of [
            () => slim.read({path: 'missing.txt'}),
            () => tools.call('mcp.some.tool', {}),
            () => tools.call('slim.nothing', {}),
            () => slim.shell({command: 'true', background: true}),
        ]) {
            try { await call(); failures.push('resolved'); } catch (error) { failures.push(error.message); }
        }
        return {listing, hits, failures};
        "#,
    )
    .await
    .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert!(
        output["listing"].as_str().unwrap().contains("a.txt"),
        "{output}"
    );
    assert!(
        output["hits"].as_str().unwrap().contains("needle here"),
        "{output}"
    );
    let failures: Vec<&str> = output["failures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|failure| failure.as_str().unwrap())
        .collect();
    assert!(failures[0].contains("missing.txt"), "{}", failures[0]);
    assert_eq!(failures[1], "no MCP servers configured");
    assert!(
        failures[2].starts_with("unknown slim tool: nothing; available: read, list"),
        "{}",
        failures[2]
    );
    assert!(
        failures[3].contains("`background` is not available"),
        "{}",
        failures[3]
    );
    // Only the calls that reached a tool used the budget.
    assert_eq!(runtime.codemode.used_calls, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_mode_has_no_slim_binding_and_refuses_native_calls() {
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 8;
    let output = eval(
        &mut runtime,
        r#"
        let message;
        try { await tools.call('slim.read', {path: 'Cargo.toml'}); } catch (error) { message = error.message; }
        return [typeof slim, message];
        "#,
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!([
            "undefined",
            "slim.<tool> functions are only available in Code mode"
        ])
    );
    assert_eq!(runtime.codemode.used_calls, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_internal_call_consumes_the_tool_budget() {
    let workspace = Workspace::new("budget");
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 2;
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const settled = await Promise.allSettled([slim.list(), slim.list(), slim.list()]);
        return settled.map(result => result.status === 'fulfilled' ? 'ok' : result.reason.message);
        "#,
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!(["ok", "ok", "tool budget exhausted; the call was not sent"])
    );
    assert_eq!(
        (
            runtime.codemode.remaining_calls,
            runtime.codemode.used_calls
        ),
        (0, 2)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_tool_adds_typescript_signatures_in_code_mode() {
    let workspace = Workspace::new("describe");
    let (mut runtime, _probe) = probe_runtime(workspace.0.join("unused"));
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        "return [describeTool('slim.read'), describeTool('mcp.probe.wait'), describeTool('slim.nothing')];",
    )
    .await
    .unwrap();
    let described: Value = serde_json::from_str(&output).unwrap();
    let native = described[0]["signature"].as_str().unwrap();
    assert!(
        native.contains("read(args: {") && native.contains("Promise<string | { path: string;"),
        "{native}"
    );
    assert_eq!(described[0]["inputSchema"]["required"], json!(["path"]));
    let mcp = described[1]["signature"].as_str().unwrap();
    assert!(
        mcp.contains("mcp.probe.wait") && mcp.contains("Promise<{ id: string }>"),
        "{mcp}"
    );
    assert_eq!(described[2], Value::Null);
    // Auto keeps the MCP description as it was.
    let auto = eval(&mut runtime, "return describeTool('mcp.probe.wait');")
        .await
        .unwrap();
    assert!(!auto.contains("signature"), "{auto}");
}

#[test]
fn definition_keeps_auto_and_declares_the_slim_api_in_code_mode() {
    let auto = definition(crate::OperatingMode::Auto, true);
    assert_eq!(auto["description"], AUTO_DESCRIPTION);
    let code = definition(crate::OperatingMode::Code, false);
    let description = code["description"].as_str().unwrap();
    assert_eq!(code["input_schema"], auto["input_schema"]);
    assert!(
        description.contains("declare const slim: {"),
        "{description}"
    );
    for native in ["read(", "list(", "search(", "glob(", "write(", "patch(", "shell("] {
        assert!(description.contains(native), "{native}: {description}");
    }
    assert!(description.contains("declare function finish("));
    assert!(description.contains("declare function describeTool("));
    assert!(!description.contains("code_intel("));
    assert!(!description.contains("background"));
    // The program chains `await slim.shell(...)` directly: `then_run` is omitted from TS decls.
    assert!(!description.contains("then_run?:"), "{description}");
    assert!(!description.contains("final?:"), "{description}");
    // Compact provider definitions: no legacy read alias.
    assert!(!description.contains(" lines?:"), "{description}");
    let with_code_intel = definition(crate::OperatingMode::Code, true);
    assert!(with_code_intel["description"]
        .as_str()
        .unwrap()
        .contains("code_intel("));
    // Deterministic, for the prompt cache.
    assert_eq!(code, definition(crate::OperatingMode::Code, false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unawaited_patches_all_run_and_late_failures_are_noted() {
    let workspace = Workspace::new("unawaited");
    std::fs::write(workspace.0.join("f.txt"), "x=0\ny=0\nz=0\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 8;
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        ['x', 'y', 'z'].forEach(async name => {
            await slim.patch({path: 'f.txt', edits: [{expected: name + '=0', replacement: name + '=done'}]});
        });
        slim.read({path: 'missing.txt'});
        return 'queued';
        "#,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("f.txt")).unwrap(),
        "x=done\ny=done\nz=done\n"
    );
    let (result, note) = output.split_once('\n').unwrap();
    assert_eq!(result, "\"queued\"");
    assert!(
        note.starts_with("calls not awaited by the program failed: slim.read (")
            && note.contains("missing.txt"),
        "{note}"
    );
    assert_eq!(runtime.codemode.used_calls, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_runs_calls_the_program_did_not_await() {
    let workspace = Workspace::new("auto-unawaited");
    let (mut runtime, probe) = probe_runtime(workspace.0.join("unused"));
    let output = eval(
        &mut runtime,
        "tools.call('mcp.probe.wait', {id: 'one', delay_ms: 50}); tools.call('mcp.probe.wait', {id: 'two'}); return 1;",
    )
    .await
    .unwrap();
    assert_eq!(output, "1");
    // Auto keeps one call at a time: `two` started after `one` ended.
    assert_eq!(probe.entry("end", "one").0, 1);
    assert_eq!(probe.entry("start", "two").0, 1);
    assert_eq!(runtime.codemode.used_calls, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edits_refuse_then_run_final_and_count_then_run_twice() {
    let workspace = Workspace::new("then-run");
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 3;
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const outcome = async call => { try { await call(); return 'ok'; } catch (error) { return error.message; } };
        return [
            await outcome(() => slim.write({path: 'a.txt', content: 'a', then_run: {command: 'true', final: 'done'}})),
            await outcome(() => slim.write({path: 'b.txt', content: 'b', then_run: {command: 'true'}})),
            await outcome(() => slim.write({path: 'c.txt', content: 'c', then_run: {command: 'true'}})),
        ];
        "#,
    )
    .await
    .unwrap();
    let outcomes: Vec<String> = serde_json::from_str(&output).unwrap();
    assert!(
        outcomes[0].contains("`then_run.final` is not available"),
        "{}",
        outcomes[0]
    );
    assert_eq!(outcomes[1], "ok");
    assert_eq!(outcomes[2], "tool budget exhausted; the call was not sent");
    assert_eq!(
        (
            runtime.codemode.remaining_calls,
            runtime.codemode.used_calls
        ),
        (1, 2)
    );
    assert!(!workspace.0.join("a.txt").exists() && !workspace.0.join("c.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_tuning_refuses_tuning_only_patches() {
    let workspace = Workspace::new("tuning");
    std::fs::write(workspace.0.join("config.txt"), "speed = 1\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 4;
    runtime.tuning_closed = true;
    let error = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        "return await slim.patch({path: 'config.txt', edits: [{expected: 'speed = 1', replacement: 'speed = 2'}]});",
    )
    .await
    .unwrap_err();
    assert!(error.contains(TUNING_CLOSED_ERROR), "{error}");
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("config.txt")).unwrap(),
        "speed = 1\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_artifacts_are_recorded_and_referenced() {
    let workspace = Workspace::new("artifact");
    let mut runtime = Runtime::with_artifact_store(workspace.0.join(".slim/artifacts")).unwrap();
    runtime
        .tools
        .configure_artifacts(runtime.artifact_store.clone(), &[]);
    runtime.codemode.remaining_calls = 4;
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        "return await slim.shell({command: 'yes artifact-line | head -n 200000'});",
    )
    .await
    .unwrap();
    assert!(
        output.contains("[artifact id="),
        "{}",
        &output[output.len().saturating_sub(300)..]
    );
    assert!(runtime
        .app
        .events()
        .iter()
        .any(|event| matches!(event.kind, crate::EventKind::ArtifactStored { .. })));
}

#[test]
fn shell_timeouts_are_capped_to_the_time_the_cell_has_left() {
    let deadline = Instant::now() + Duration::from_secs(10);
    for (arguments, expected) in [
        (json!({"command": "true"}), json!(10_000)),
        (
            json!({"command": "true", "timeout_ms": 60_000}),
            json!(10_000),
        ),
        (json!({"command": "true", "timeout_ms": 500}), json!(500)),
        (json!({"command": "true", "timeout_ms": "x"}), json!("x")),
    ] {
        let mut capped = arguments;
        calls::cap_shell_timeout(&mut capped, deadline).unwrap();
        let timeout = &capped["timeout_ms"];
        match expected.as_u64() {
            Some(10_000) => {
                assert!(
                    (9_000..=10_000).contains(&timeout.as_u64().unwrap()),
                    "{timeout}"
                )
            }
            _ => assert_eq!(timeout, &expected),
        }
    }
    assert!(calls::cap_shell_timeout(&mut json!({}), Instant::now()).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_journal_failure_still_runs_and_drains_started_calls() {
    let workspace = Workspace::new("journal-failure");
    let path = workspace.0.join("session.jsonl");
    let repo = JsonlRepo::create(
        &path,
        DurableSessionHeader::new("code", "now", workspace.0.to_string_lossy(), None, None),
    )
    .unwrap();
    let journal = Arc::new(Mutex::new(
        ManualRunJournal::start(
            repo,
            ManualRunSpec::new("one", "attempt", "input", "final", "write", 0),
        )
        .unwrap(),
    ));
    // A poisoned lock makes every journal write fail.
    let poisoner = Arc::clone(&journal);
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.lock().unwrap();
        panic!("poison the journal lock");
    })
    .join();
    let mut runtime = Runtime::new();
    runtime.app.set_run_journal(journal);
    // Past restore, so the first failing write is the call's `started` fact.
    runtime.codemode.restored = true;
    runtime.codemode.remaining_calls = 4;
    let arguments =
        json!({"code": "await slim.write({path: 'out.txt', content: 'x'}); return 1;"}).to_string();
    let result = runtime
        .run_codemode(
            crate::OperatingMode::Code,
            &workspace.0,
            ToolInvocation {
                batch_id: "cell",
                call_id: "journal",
                name: "codemode",
                arguments: &arguments,
            },
            &mut 0,
            Duration::from_secs(10),
        )
        .await;
    assert!(result.is_err(), "{result:?}");
    // The call that took budget ran, and its outcome was collected.
    assert_eq!(runtime.codemode.used_calls, 1);
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("out.txt")).unwrap(),
        "x"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_failures_are_journaled_with_size_and_a_bounded_prefix() {
    let workspace = Workspace::new("failure-fact");
    let repo = JsonlRepo::create(
        workspace.0.join("session.jsonl"),
        DurableSessionHeader::new("code", "now", workspace.0.to_string_lossy(), None, None),
    )
    .unwrap();
    let journal = Arc::new(Mutex::new(
        ManualRunJournal::start(
            repo,
            ManualRunSpec::new("one", "attempt", "input", "final", "write", 0),
        )
        .unwrap(),
    ));
    let mut runtime = Runtime::new();
    runtime.app.set_run_journal(Arc::clone(&journal));
    runtime.codemode.remaining_calls = 4;
    let code = "try { await slim.shell({command: 'yes failure-line | head -n 400; exit 3'}); } catch (error) { return error.message; }";
    let output = eval_in(&mut runtime, crate::OperatingMode::Code, &workspace.0, code)
        .await
        .unwrap();
    let message: String = serde_json::from_str(&output).unwrap();
    let journal = journal.lock().unwrap();
    let fact = journal
        .repo()
        .records()
        .iter()
        .rev()
        .find_map(|record| match record {
            DurableRecord::Fact { fact, .. } if fact.namespace == CALL_NAMESPACE => {
                Some(&fact.value)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(fact["status"], "failed");
    assert_eq!(fact["error_bytes"], json!(message.len()));
    assert_eq!(
        fact["error"].as_str().unwrap(),
        calls::prefix(&message, calls::MAX_JOURNALED_ERROR_BYTES)
    );
    // Longer errors keep at most 1 KiB, cut on a character boundary.
    let long = "é".repeat(1000);
    let cut = calls::prefix(&long, calls::MAX_JOURNALED_ERROR_BYTES);
    assert_eq!((cut.len(), long.len()), (1024, 2000));
    assert_eq!(calls::prefix("ab€", 3), "ab");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_internal_call_is_shown_as_a_step_of_the_cell() {
    let workspace = Workspace::new("steps");
    std::fs::write(workspace.0.join("f.txt"), "x=0\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 4;
    eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        await slim.patch({path: 'f.txt', edits: [{expected: 'x=0', replacement: 'x=1'}]});
        try { await slim.read({path: 'missing.txt'}); } catch (error) {}
        return 'ok';
        "#,
    )
    .await
    .unwrap();
    let mut started = Vec::new();
    let mut finished = Vec::new();
    for event in runtime.app.events() {
        match &event.kind {
            crate::EventKind::ToolStepStarted {
                batch_id,
                call_id,
                step_id,
                name,
                arguments,
            } => {
                assert_eq!((batch_id.as_str(), call_id.as_str()), ("cell", "code"));
                started.push((step_id.clone(), name.clone(), arguments.clone()));
            }
            crate::EventKind::ToolStepFinished {
                step_id,
                name,
                success,
                output,
                diff,
                ..
            } => finished.push((
                step_id.clone(),
                name.clone(),
                *success,
                output.clone(),
                diff.clone(),
            )),
            _ => {}
        }
    }
    assert_eq!(started.len(), 2, "{started:?}");
    assert_eq!(started[0].1, "slim.patch");
    assert!(
        started[0].2.contains("\"path\":\"f.txt\""),
        "{}",
        started[0].2
    );
    assert_eq!(started[1].1, "slim.read");
    assert_eq!(finished.len(), 2);
    let (patch_id, _, patched, _, diff) = &finished[0];
    assert_eq!(
        patch_id, &started[0].0,
        "a step ends under the id it started"
    );
    assert!(*patched);
    assert_eq!(diff.as_ref().map(|diff| diff.path.as_str()), Some("f.txt"));
    let (_, name, read, output, diff) = &finished[1];
    assert_eq!(name, "slim.read");
    assert!(!read && diff.is_none());
    assert!(output.contains("missing.txt"), "{output}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_calls_do_not_consume_mutating_budget_while_writes_and_mcp_do() {
    let workspace = Workspace::new("read-vs-mut");
    std::fs::write(workspace.0.join("a.txt"), "hello\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 10;
    runtime.codemode.remaining_mutating_calls = Some(1);
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const r1 = await slim.read({path: 'a.txt'});
        const r2 = await slim.list();
        await slim.write({path: 'b.txt', content: r1});
        let blocked;
        try { await slim.write({path: 'c.txt', content: 'x'}); } catch (e) { blocked = e.message; }
        return {r2, blocked};
        "#,
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(parsed["blocked"].as_str().unwrap().contains("tool budget exhausted"));
    assert_eq!(runtime.codemode.remaining_mutating_calls, Some(0));
    assert_eq!(runtime.codemode.used_calls, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_call_journals_compact_result_bytes_without_raw_payload() {
    let workspace = Workspace::new("mcp-journal-bytes");
    let (mut mcp_runtime, _probe) = probe_runtime(workspace.0.join("unused"));
    let repo = JsonlRepo::create(
        workspace.0.join("session.jsonl"),
        DurableSessionHeader::new("code", "now", workspace.0.to_string_lossy(), None, None),
    )
    .unwrap();
    let journal = Arc::new(Mutex::new(
        ManualRunJournal::start(
            repo,
            ManualRunSpec::new("one", "attempt", "input", "final", "write", 0),
        )
        .unwrap(),
    ));
    mcp_runtime.app.set_run_journal(Arc::clone(&journal));
    mcp_runtime.codemode.remaining_calls = 4;
    eval_in(
        &mut mcp_runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        "return await tools.call('mcp.probe.wait', {id: 'small'});",
    )
    .await
    .unwrap();
    let journal = journal.lock().unwrap();
    let fact = journal
        .repo()
        .records()
        .iter()
        .rev()
        .find_map(|record| match record {
            DurableRecord::Fact { fact, .. } if fact.namespace == CALL_NAMESPACE => {
                Some(&fact.value)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(fact["status"], "completed");
    assert!(fact["result"]["result_bytes"].as_u64().unwrap() > 0);
    assert!(fact["result"].get("id").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn distinct_file_mutations_run_concurrently_while_same_file_patches_serialize() {
    let workspace = Workspace::new("mut-waves");
    std::fs::write(workspace.0.join("a.txt"), "a=0\nx=0\n").unwrap();
    std::fs::write(workspace.0.join("b.txt"), "b=0\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 8;
    eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        await Promise.all([
            slim.patch({path: 'a.txt', edits: [{expected: 'a=0', replacement: 'a=1'}]}),
            slim.patch({path: 'b.txt', edits: [{expected: 'b=0', replacement: 'b=1'}]}),
        ]);
        await Promise.all([
            slim.patch({path: 'a.txt', edits: [{expected: 'a=1', replacement: 'a=2'}]}),
            slim.patch({path: 'a.txt', edits: [{expected: 'x=0', replacement: 'x=2'}]}),
        ]);
        return 'ok';
        "#,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("a.txt")).unwrap(),
        "a=2\nx=2\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("b.txt")).unwrap(),
        "b=1\n"
    );
    let step_events: Vec<_> = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ToolStepStarted { step_id, .. } => {
                Some(("start", step_id.as_str()))
            }
            crate::EventKind::ToolStepFinished { step_id, .. } => {
                Some(("finish", step_id.as_str()))
            }
            _ => None,
        })
        .collect();
    // First wave (a.txt + b.txt) starts both before either finishes; second wave (same a.txt) serializes.
    assert!(step_events.len() >= 8, "{step_events:?}");
    let first_finish = step_events
        .iter()
        .position(|(kind, _)| *kind == "finish")
        .unwrap();
    assert!(
        first_finish >= 2,
        "distinct-file patches must overlap: {step_events:?}"
    );
    let second_wave_starts: Vec<_> = step_events[4..]
        .iter()
        .enumerate()
        .filter_map(|(idx, (kind, _))| (*kind == "start").then_some(idx + 4))
        .collect();
    let second_wave_finishes: Vec<_> = step_events[4..]
        .iter()
        .enumerate()
        .filter_map(|(idx, (kind, _))| (*kind == "finish").then_some(idx + 4))
        .collect();
    assert!(
        second_wave_finishes[0] < second_wave_starts[1],
        "same-file patches must serialize: {step_events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_lists_matching_files_and_rejects_escape_or_negation() {
    let workspace = Workspace::new("glob-native");
    std::fs::create_dir_all(workspace.0.join("src/nested")).unwrap();
    std::fs::write(workspace.0.join("src/a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(workspace.0.join("src/nested/b.rs"), "pub fn b() {}\n").unwrap();
    std::fs::write(workspace.0.join("README.md"), "# readme\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 6;
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const allRs = await slim.glob({pattern: '**/*.rs'});
        const limited = await slim.glob({pattern: '*.rs', path: 'src', max_entries: 1});
        const errs = [];
        for (const args of [{pattern: '../outside/*'}, {path: '../outside', pattern: '*.rs'}, {pattern: '!*.rs'}]) {
            try { await slim.glob(args); errs.push('ok'); } catch (e) { errs.push(e.message); }
        }
        return {allRs, limited, errs};
        "#,
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    let files = parsed["allRs"]["files"].as_array().unwrap();
    assert!(files.iter().any(|v| v == "src/a.rs"), "{parsed}");
    assert!(files.iter().any(|v| v == "src/nested/b.rs"), "{parsed}");
    assert!(!files.iter().any(|v| v == "README.md"), "{parsed}");
    assert_eq!(parsed["limited"]["files"], json!(["src/a.rs"]));
    assert_eq!(parsed["limited"]["truncated"], json!(true));
    assert_ne!(parsed["errs"][0], "ok");
    assert_ne!(parsed["errs"][1], "ok");
    assert_ne!(parsed["errs"][2], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_search_and_list_return_parsed_redacted_objects_and_allow_non_zero_shell() {
    let workspace = Workspace::new("structured-shell");
    std::fs::write(
        workspace.0.join("secret.txt"),
        "token=secret-fixture-token\nother=ok\n",
    )
    .unwrap();
    let mut runtime = Runtime::new();
    runtime.register_sensitive_value("secret-fixture-token");
    runtime.codemode.remaining_calls = 6;
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const listed = await slim.list({structured: true});
        const searched = await slim.search({query: 'token=', structured: true});
        const nonZero = await slim.shell({command: 'exit 3', allow_non_zero: true});
        let failedDefault = null;
        try { await slim.shell({command: 'exit 4'}); } catch (e) { failedDefault = e.message; }
        return {
            listType: typeof listed,
            hasEntries: Array.isArray(listed.entries),
            searchType: typeof searched,
            searched,
            nonZero,
            failedDefault,
        };
        "#,
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["listType"], "object");
    assert_eq!(parsed["hasEntries"], json!(true));
    assert_eq!(parsed["searchType"], "object");
    assert!(!output.contains("secret-fixture-token"), "{output}");
    assert!(
        parsed["nonZero"].as_str().unwrap().contains("exit 3"),
        "{parsed}"
    );
    assert!(
        parsed["failedDefault"]
            .as_str()
            .unwrap()
            .contains("exit 4"),
        "{parsed}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_patch_compacts_journal_started_arguments_and_keeps_edit_line_stats_and_rewrite_note()
{
    let workspace = Workspace::new("compact-journal-notes");
    let big = format!("line0\nline1\n{}", "keep\n".repeat(600));
    std::fs::write(workspace.0.join("big.txt"), &big).unwrap();
    let repo = JsonlRepo::create(
        workspace.0.join("session.jsonl"),
        DurableSessionHeader::new("code", "now", workspace.0.to_string_lossy(), None, None),
    )
    .unwrap();
    let journal = Arc::new(Mutex::new(
        ManualRunJournal::start(
            repo,
            ManualRunSpec::new("one", "attempt", "input", "final", "patch", 0),
        )
        .unwrap(),
    ));
    let mut runtime = Runtime::new();
    runtime.app.set_run_journal(Arc::clone(&journal));
    runtime.codemode.remaining_calls = 4;
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const body = 'line0\nline1\n' + 'keep\n'.repeat(600);
        await slim.write({path: 'big.txt', expected: body, content: body});
        const replacement = 'new0\n' + 'pad line for size\n'.repeat(80);
        await slim.patch({
            path: 'big.txt',
            edits: [{expected: 'line0\nline1\n', replacement}],
        });
        return 'done';
        "#,
    )
    .await
    .unwrap();
    assert!(
        output.contains("You rewrote all of big.txt."),
        "expected RunGuards rewrite advisory in cell output: {output}"
    );
    let journal = journal.lock().unwrap();
    let patch_started = journal
        .repo()
        .records()
        .iter()
        .find_map(|record| match record {
            DurableRecord::Fact { fact, .. }
                if fact.namespace == CALL_NAMESPACE
                    && fact.value["name"] == "slim.patch"
                    && fact.value["status"] == "started" =>
            {
                Some(&fact.value)
            }
            _ => None,
        })
        .expect("patch started fact");
    assert_eq!(patch_started["arguments"]["elided"], json!(true));
    assert_eq!(patch_started["arguments"]["path"], json!("big.txt"));
    assert!(
        patch_started["arguments"]["edits"]
            .as_str()
            .is_some_and(|summary| summary.ends_with(" bytes>")),
        "{patch_started}"
    );
    assert_eq!(
        crate::tools::edit_line_stats("patch", &patch_started["arguments"].to_string()),
        Some((81, 2))
    );
}

#[test]
fn definition_for_omits_mcp_and_code_intel_when_disabled() {
    let local = definition_for(crate::OperatingMode::Code, false, false);
    let local_desc = local["description"].as_str().unwrap();
    assert!(!local_desc.contains("listServers("), "{local_desc}");
    assert!(!local_desc.contains("searchTools("), "{local_desc}");
    assert!(!local_desc.contains("describeTool("), "{local_desc}");
    assert!(!local_desc.contains("tools.call("), "{local_desc}");
    assert!(!local_desc.contains("code_intel"), "{local_desc}");
    assert!(local_desc.contains("declare function finish("), "{local_desc}");

    let full = definition_for(crate::OperatingMode::Code, true, true);
    let full_desc = full["description"].as_str().unwrap();
    assert!(full_desc.contains("listServers("));
    assert!(full_desc.contains("code_intel("));
    assert!(
        local_desc.len() + 400 < full_desc.len(),
        "local schema ({}) should save substantial tokens vs full ({})",
        local_desc.len(),
        full_desc.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_non_zero_shell_is_not_held_by_run_guards_on_immediate_repeat() {
    let workspace = Workspace::new("allow-non-zero-repeat");
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 6;
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const first = await slim.shell({ command: 'exit 2', allow_non_zero: true });
        const second = await slim.shell({ command: 'exit 2', allow_non_zero: true });
        return { first, second };
        "#,
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(parsed["first"].as_str().unwrap().contains("exit 2"), "{parsed}");
    assert!(parsed["second"].as_str().unwrap().contains("exit 2"), "{parsed}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_mcp_mutation_never_leaves_stale_read_cache_entry() {
    struct MutatingMcp {
        target: std::path::PathBuf,
    }
    #[async_trait::async_trait]
    impl crate::mcp::McpConnection for MutatingMcp {
        async fn request(
            &self,
            _method: &str,
            _params: Value,
        ) -> Result<Value, crate::mcp::McpError> {
            // Wait long enough for a parallel slim.read to complete first,
            // then mutate the file before returning.
            tokio::time::sleep(Duration::from_millis(80)).await;
            std::fs::write(&self.target, "after-mcp\n").unwrap();
            Ok(json!({"content": [], "structuredContent": {"mutated": true}}))
        }
        async fn notify(&self, _method: &str, _params: Value) {}
        fn is_closed(&self) -> bool {
            false
        }
    }

    let workspace = Workspace::new("mcp-stale-cache");
    let target = workspace.0.join("data.txt");
    std::fs::write(&target, "before-mcp\n").unwrap();
    let manager = Arc::new(McpManager::new(
        Default::default(),
        workspace.0.clone(),
        Default::default(),
    ));
    manager.insert_connection(
        crate::mcp::McpServerSpec::new(
            "mut",
            crate::mcp::McpTransport::Stdio {
                command: "test".into(),
                args: Vec::new(),
                env: Default::default(),
            },
        ),
        Arc::new(MutatingMcp { target }),
        vec![crate::mcp::McpToolSummary {
            name: "touch".into(),
            description: Some("Mutate file".into()),
            schema: json!({"type": "object"}),
            output_schema: None,
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: false,
        }],
    );
    let mut runtime = Runtime::new();
    runtime.set_mcp_manager(Some(manager));
    runtime.codemode.remaining_calls = 8;
    let output = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const [_, during] = await Promise.all([
            tools.call('mcp.mut.touch', {}),
            slim.read({ path: 'data.txt' }),
        ]);
        const after = await slim.read({ path: 'data.txt' });
        return { during, after };
        "#,
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["during"], json!("before-mcp\n"));
    assert_eq!(
        parsed["after"],
        json!("after-mcp\n"),
        "read after overlapping MCP mutation must not return stale cached content"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disjoint_file_mutation_is_not_head_of_line_blocked_by_same_file_second_patch() {
    let workspace = Workspace::new("hol-mut-wave");
    std::fs::write(workspace.0.join("a.txt"), "a=0\nx=0\n").unwrap();
    std::fs::write(workspace.0.join("b.txt"), "b=0\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 8;
    eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        await Promise.all([
            slim.patch({ path: 'a.txt', edits: [{ expected: 'a=0', replacement: 'a=1' }] }),
            slim.patch({ path: 'a.txt', edits: [{ expected: 'x=0', replacement: 'x=1' }] }),
            slim.patch({ path: 'b.txt', edits: [{ expected: 'b=0', replacement: 'b=1' }] }),
        ]);
        return 'ok';
        "#,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("a.txt")).unwrap(),
        "a=1\nx=1\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("b.txt")).unwrap(),
        "b=1\n"
    );
    let started_paths: Vec<String> = runtime
        .app
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            crate::EventKind::ToolStepStarted { arguments, .. } => {
                let v: Value = serde_json::from_str(arguments).ok()?;
                Some(v["path"].as_str()?.to_owned())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        started_paths,
        vec!["a.txt", "b.txt", "a.txt"],
        "b.txt must start immediately alongside the first a.txt patch instead of waiting behind the second a.txt patch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_after_workspace_mutation_does_not_fire_reusable_evidence_anomaly() {
    let workspace = Workspace::new("glob-governor-rev");
    std::fs::write(workspace.0.join("a.rs"), "pub fn a() {}\n").unwrap();
    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 8;
    let mut governor = CausalGovernor::default();
    let args = json!({
        "code": r#"
            const first = await slim.glob({ pattern: '*.rs' });
            await slim.write({ path: 'README.md', content: '# doc\n' });
            const second = await slim.glob({ pattern: '*.rs' });
            return { first: first.files, second: second.files };
        "#
    })
    .to_string();
    let (result, _) = runtime
        .execute_codemode_with_governor(
            crate::OperatingMode::Code,
            &workspace.0,
            ToolInvocation {
                batch_id: "b0",
                call_id: "c0",
                name: "codemode",
                arguments: &args,
            },
            0,
            Some(&mut governor),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    let anomalies: Vec<_> = runtime
        .app
        .events()
        .iter()
        .filter(|event| matches!(event.kind, crate::EventKind::CausalAnomalyDetected { .. }))
        .collect();
    assert!(
        anomalies.is_empty(),
        "glob after workspace mutation must not trigger ReusableEvidence: {anomalies:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prelude_stdlib_helpers_and_read_patch_many_work() {
    let workspace = Workspace::new("stdlib-many");
    std::fs::write(workspace.0.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    std::fs::write(workspace.0.join("b.txt"), "one\ntwo\nthree\n").unwrap();

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 12;
    let out = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const files = await slim.readMany(['a.txt', 'b.txt']);
        const g = grepText(files['a.txt'], /be/, { context: 1 });
        const d = diffText(files['a.txt'], files['a.txt'].replace('beta', 'BETA'));
        const patched = await slim.patchMany([
            { path: 'a.txt', edits: [{ expected: 'beta', replacement: 'BETA' }] },
            { path: 'b.txt', edits: [{ expected: 'two', replacement: 'TWO' }] },
        ]);
        return {
            aLines: lines(files['a.txt']),
            grep: g,
            diff: d,
            parsedOk: parseJsonSafe('{"ok":true}'),
            parsedFallback: parseJsonSafe('bad', { fallback: 1 }),
            chunks: chunk([1, 2, 3, 4, 5], 2),
            uniq: unique([{ id: 1, v: 'x' }, { id: 1, v: 'y' }, { id: 2, v: 'z' }], x => x.id),
            patchedCount: patched.length,
        };
        "#,
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["aLines"], json!(["alpha", "beta", "gamma", ""]));
    assert_eq!(v["grep"][0]["line"], 2);
    assert_eq!(v["grep"][0]["context"].as_array().unwrap().len(), 2);
    assert!(v["diff"].as_str().unwrap().contains("-beta"));
    assert!(v["diff"].as_str().unwrap().contains("+BETA"));
    assert_eq!(v["parsedOk"], json!({"ok": true}));
    assert_eq!(v["parsedFallback"], json!({"fallback": 1}));
    assert_eq!(v["chunks"], json!([[1, 2], [3, 4], [5]]));
    assert_eq!(v["uniq"], json!([{"id": 1, "v": "x"}, {"id": 2, "v": "z"}]));
    assert_eq!(v["patchedCount"], 2);
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("a.txt")).unwrap(),
        "alpha\nBETA\ngamma\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("b.txt")).unwrap(),
        "one\nTWO\nthree\n"
    );
}

#[tokio::test]
async fn typescript_annotations_strip_cleanly_on_syntax_error_fallback() {
    let mut runtime = Runtime::new();
    let out = eval(
        &mut runtime,
        r#"
        interface Pair { key: string; count: number }
        type Items = Pair[];
        const formatItem = (item: Pair, index?: number): string => `${index ?? 0}:${item.key}=${item.count}`;
        function sumCounts(items: readonly Pair[]): number {
            return items.reduce((acc: number, row: Pair) => acc + row.count, 0);
        }
        const rows: Items = [{ key: 'a', count: 2 }, { key: 'b', count: 5 }] as Items;
        return { first: formatItem(rows[0]), total: sumCounts(rows) };
        "#,
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed, json!({ "first": "0:a=2", "total": 7 }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_read_cache_persists_across_cells_and_invalidates_on_mutation() {
    let workspace = Workspace::new("session-cache");
    std::fs::write(workspace.0.join("note.txt"), "v1\n").unwrap();

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 12;

    let first = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        "return await slim.read({ path: 'note.txt' });",
    )
    .await
    .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&first).unwrap(), json!("v1\n"));
    assert!(!runtime.codemode.session_read_cache.is_empty());

    // Same workspace_revision -> second cell reuses session_read_cache.
    let second = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        "return await slim.read({ path: 'note.txt' });",
    )
    .await
    .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&second).unwrap(), json!("v1\n"));

    // Mutating clears session_read_cache and bumps workspace revision; read sees v2.
    let third = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        await slim.write({ path: 'note.txt', content: 'v2\n', expected: 'v1\n' });
        return await slim.read({ path: 'note.txt' });
        "#,
    )
    .await
    .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&third).unwrap(), json!("v2\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_read_and_git_tools_return_typed_json() {
    let workspace = Workspace::new("git-structured");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&workspace.0)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {:?}", String::from_utf8_lossy(&out.stderr));
    };
    run_git(&["init"]);
    run_git(&["config", "user.name", "test"]);
    run_git(&["config", "user.email", "test@example.com"]);
    std::fs::write(workspace.0.join("src.rs"), "fn main() {}\n// second\n").unwrap();
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "initial commit"]);
    std::fs::write(workspace.0.join("src.rs"), "fn main() { println!(\"hi\"); }\n// second\n").unwrap();

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 10;
    let out = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const [doc, st, df, lg] = await Promise.all([
            slim.read({ path: 'src.rs', structured: true }),
            slim.git({ op: 'status' }),
            slim.git({ op: 'diff', path: 'src.rs' }),
            slim.git({ op: 'log', limit: 5 }),
        ]);
        return { doc, st, df, lg };
        "#,
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["doc"]["path"], "src.rs");
    assert_eq!(v["doc"]["first_line"], 1);
    assert_eq!(v["doc"]["total_lines"], 2);
    assert_eq!(v["doc"]["complete"], true);
    assert_eq!(v["doc"]["lines"][1], "// second");
    assert_eq!(v["st"]["clean"], false);
    assert_eq!(v["st"]["entries"][0]["path"], "src.rs");
    assert!(v["df"]["diff"].as_str().unwrap().contains("println!"));
    assert_eq!(v["lg"]["commits"][0]["subject"], "initial commit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_rolls_back_file_writes_on_throw_and_failure_reports_applied_effects() {
    let workspace = Workspace::new("atomic-effects");
    std::fs::write(workspace.0.join("a.txt"), "orig\n").unwrap();

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 10;
    let atomic_err = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        await slim.atomic(async () => {
            await slim.write({ path: 'a.txt', content: 'mutated\n', expected: 'orig\n' });
            await slim.write({ path: 'temp.txt', content: 'temp\n', expected: null });
            throw new Error('abort tx');
        });
        "#,
    )
    .await
    .unwrap_err();
    assert!(atomic_err.contains("abort tx"), "{atomic_err}");
    assert_eq!(std::fs::read_to_string(workspace.0.join("a.txt")).unwrap(), "orig\n");
    assert!(!workspace.0.join("temp.txt").exists());

    // Outside atomic: applied_effects summarizes durable mutations before a later failure.
    let args = json!({
        "code": r#"
            await slim.write({ path: 'kept.txt', content: 'saved\n', expected: null });
            throw new Error('fail after write');
        "#
    })
    .to_string();
    let next_seq = runtime.app.events().last().map_or(0, |event| event.seq + 1);
    let (res, _) = runtime
        .execute_codemode_with_governor(
            crate::OperatingMode::Code,
            &workspace.0,
            ToolInvocation {
                batch_id: "b1",
                call_id: "c1",
                name: "codemode",
                arguments: &args,
            },
            next_seq,
            None,
        )
        .await
        .unwrap();
    assert!(!res.success);
    assert!(res.output.contains("applied_effects:"), "{}", res.output);
    assert!(res.output.contains("kept.txt"), "{}", res.output);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readonly_shell_and_disjoint_read_overlap_and_codemode_subagent_work() {
    struct EchoSpawner;
    impl crate::runtime::native_subagent::SubagentSpawner for EchoSpawner {
        fn run(
            &self,
            args: crate::runtime::native_subagent::SubagentRunArgs,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'static>>
        {
            Box::pin(async move { Ok(format!("echo:{}:{}", args.role, args.prompt)) })
        }
    }

    let workspace = Workspace::new("parallel-ro-subagent");
    std::fs::write(workspace.0.join("read_me.txt"), "hello world\n").unwrap();

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 12;
    runtime.codemode.remaining_mutating_calls = Some(1);
    let reg = crate::runtime::SubagentRegistry::new();
    reg.set_spawner(Arc::new(EchoSpawner), crate::OperatingMode::Code);
    runtime.set_subagents(reg);

    // Note remaining_mutating_calls == 1:
    // - `write_me.txt` consumes that 1 mutating slot.
    // - `slim.shell({ readonly: true })` and `slim.subagent({ readonly: true })` do NOT fail mutating budget!
    // - `read_me.txt` is disjoint from `write_me.txt` and overlaps cleanly.
    let out = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const [wrote, read, sh, sub] = await Promise.all([
            slim.write({ path: 'write_me.txt', content: 'written\n', expected: null }),
            slim.read({ path: 'read_me.txt' }),
            slim.shell({ command: 'echo readonly-ok', readonly: true }),
            slim.subagent({ role: 'Check', prompt: 'verify', readonly: true }),
        ]);
        const st = await slim.subagent_control({ action: 'status' });
        return { wrote, read, sh, sub, st };
        "#,
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["read"], "hello world\n");
    assert!(v["sh"].as_str().unwrap().contains("readonly-ok"));
    assert!(v["sub"].as_str().unwrap().contains("echo:Check:verify"));
    assert!(v["st"].as_str().unwrap().contains("Check"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_read_cache_hits_for_glob_and_git_and_batch_primitives_work_in_ram() {
    let workspace = Workspace::new("glob-git-ram");
    let canon = std::fs::canonicalize(&workspace.0).unwrap();
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&canon)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
    };
    run_git(&["init"]);
    run_git(&["config", "user.name", "test"]);
    run_git(&["config", "user.email", "test@example.com"]);
    std::fs::write(canon.join("alpha.rs"), "pub fn compute_total(n: usize) -> usize { n + 1 }\n").unwrap();
    std::fs::write(canon.join("beta.rs"), "pub fn call_site() -> usize { compute_total(3) }\n").unwrap();
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "init"]);

    let daemon = crate::project_index::for_workspace(&canon).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline && !daemon.stats().graph_ready {
        std::thread::sleep(std::time::Duration::from_millis(15));
    }

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 20;

    // Cell 1 populates SessionReadCache for slim.glob and slim.git even when daemon is active.
    let first = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &canon,
        r#"
        const [gl, st, br, many, outlines, grep, callers] = await Promise.all([
            slim.glob({ pattern: '*.rs' }),
            slim.git({ op: 'status' }),
            slim.git({ op: 'branch' }),
            slim.readMany(['alpha.rs', 'beta.rs']),
            slim.outlineMany(['alpha.rs', 'beta.rs']),
            slim.grepIndex({ queries: ['compute_total'] }),
            slim.graph({ op: 'callers', symbol: 'compute_total' }),
        ]);
        return { gl, st, br, many, outlines, grep, callers };
        "#,
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(v["gl"]["files"].as_array().unwrap().len(), 2);
    assert_eq!(v["st"]["clean"], true);
    assert!(!v["br"]["branches"].as_array().unwrap().is_empty());
    assert!(v["many"]["alpha.rs"].as_str().unwrap().contains("compute_total"));
    assert_eq!(v["outlines"]["alpha.rs"][0]["name"], "compute_total");
    assert!(!v["grep"]["matches"].as_array().unwrap().is_empty());
    assert!(v["callers"]["callers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["rel_path"] == "beta.rs"));

    // Verify glob & git entries actually landed in session_read_cache and hit on cell 2.
    assert!(
        runtime.codemode.session_read_cache.entries.len() >= 2,
        "session_read_cache should retain glob/git entries when daemon epoch matches"
    );
    let second = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &canon,
        "return await Promise.all([slim.glob({ pattern: '*.rs' }), slim.git({ op: 'status' })]);",
    )
    .await
    .unwrap();
    let v2: Value = serde_json::from_str(&second).unwrap();
    assert_eq!(v2[0]["files"], v["gl"]["files"]);
    assert_eq!(v2[1]["clean"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_code_hybrid_runs_in_auto_and_readonly_in_plan() {
    let workspace = Workspace::new("batch-code-hybrid");
    std::fs::write(workspace.0.join("item.txt"), "hello\n").unwrap();

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 12;

    let inv_auto = ToolInvocation {
        batch_id: "b1",
        call_id: "c1",
        name: "batch_code",
        arguments: &json!({
            "code": r#"
            const before = await slim.readMany(['item.txt']);
            await slim.write({ path: 'created.txt', content: 'from-batch\n', expected: null });
            return before['item.txt'];
            "#
        })
        .to_string(),
    };
    let (res_auto, next_seq) = runtime
        .execute_codemode_with_governor(
            crate::OperatingMode::Auto,
            &workspace.0,
            inv_auto,
            1,
            None,
        )
        .await
        .unwrap();
    assert!(res_auto.success, "{}", res_auto.output);
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("created.txt")).unwrap(),
        "from-batch\n"
    );

    // In Plan mode: readMany works, write/shell are refused.
    let inv_plan = ToolInvocation {
        batch_id: "b2",
        call_id: "c2",
        name: "batch_code",
        arguments: &json!({
            "code": r#"
            const files = await slim.readMany(['item.txt', 'created.txt']);
            let writeType = typeof slim.write;
            let refused = '';
            try { await tools.call('slim.write', { path: 'bad.txt', content: 'no' }); } catch (e) { refused = e.message; }
            return { files, writeType, refused };
            "#
        })
        .to_string(),
    };
    let (res_plan, _) = runtime
        .execute_codemode_with_governor(
            crate::OperatingMode::Plan,
            &workspace.0,
            inv_plan,
            next_seq,
            None,
        )
        .await
        .unwrap();
    assert!(res_plan.success, "{}", res_plan.output);
    let parsed: Value = serde_json::from_str(&res_plan.output).unwrap();
    assert_eq!(parsed["files"]["created.txt"], "from-batch\n");
    assert_eq!(parsed["writeType"], "undefined");
    assert!(parsed["refused"].as_str().unwrap().contains("Read-only or Plan"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_index_fallback_short_and_case_insensitive_queries_return_matches() {
    let workspace = Workspace::new("grep-index-fallback");
    std::fs::write(workspace.0.join("sample.rs"), "pub fn HelloWorld() {}\nlet ok = 1;\n").unwrap();

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 10;
    let out = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        return await slim.grepIndex({
            queries: ['fn', 'helloworld'],
            case_insensitive: true,
        });
        "#,
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let matches = v["matches"].as_array().unwrap();
    assert!(
        matches.iter().any(|m| m["query"] == "fn" && m["path"] == "sample.rs" && m["line"] == 1),
        "short query fallback should match: {v}"
    );
    assert!(
        matches.iter().any(|m| m["query"] == "helloworld" && m["path"] == "sample.rs" && m["line"] == 1),
        "case_insensitive fallback should match: {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_structured_read_large_file_lazy_lines_raw_over_1000_and_post_patch_reread() {
    let workspace = Workspace::new("code-read-no-auto-leak");
    let mut big = String::new();
    for i in 1..=1150 {
        use std::fmt::Write as _;
        let _ = writeln!(big, "pub fn item_{i:04}() -> usize {{ {i} }} // padding line");
    }
    assert!(big.len() > 12 * 1024);
    std::fs::write(workspace.0.join("big.rs"), &big).unwrap();
    std::fs::write(workspace.0.join("small.rs"), "const VALUE = 1;\n").unwrap();

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 20;
    let out = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &workspace.0,
        r#"
        const [one, many, fullRaw] = await Promise.all([
            slim.read({ path: 'big.rs', structured: true }),
            slim.readMany(['big.rs'], { structured: true }),
            slim.read({ path: 'big.rs', raw: true }),
        ]);
        await slim.patch({
            path: 'small.rs',
            edits: [{ expected: 'const VALUE = 1;', replacement: 'const VALUE = 2;' }],
        });
        const reread = await slim.read({ path: 'small.rs' });
        return {
            onePath: one.path,
            oneHasLines: Array.isArray(one.lines) && one.lines.length > 0,
            oneFirst: one.lines[0],
            oneDocJson: one,
            manyLine0: many['big.rs'].lines[0],
            rawHasLastLine: fullRaw.includes('item_1150'),
            reread,
        };
        "#,
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["onePath"], "big.rs");
    assert_eq!(v["oneHasLines"], true);
    assert!(v["oneFirst"].as_str().unwrap().contains("item_0001"));
    assert!(v["oneDocJson"]["lines"].is_array());
    assert!(v["manyLine0"].as_str().unwrap().contains("item_0001"));
    assert_eq!(v["rawHasLastLine"], true);
    assert_eq!(v["reread"], "const VALUE = 2;\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_rollback_syncs_index_and_read_cache_and_batch_primitives_cache() {
    let workspace = Workspace::new("atomic-sync-batch-cache");
    let canon = std::fs::canonicalize(&workspace.0).unwrap();
    std::fs::write(canon.join("target.rs"), "pub fn orig_token() -> usize { 1 }\n").unwrap();
    std::fs::write(canon.join("other.rs"), "pub fn sibling() -> usize { 2 }\n").unwrap();

    let daemon = crate::project_index::for_workspace(&canon).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline && !daemon.stats().graph_ready {
        std::thread::sleep(std::time::Duration::from_millis(15));
    }

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 30;
    let err = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &canon,
        r#"
        await slim.atomic(async () => {
            await slim.patch({
                path: 'target.rs',
                edits: [{ expected: 'orig_token', replacement: 'rolled_token' }],
            });
            throw new Error('abort tx');
        });
        "#,
    )
    .await
    .unwrap_err();
    assert!(err.contains("abort tx"), "{err}");

    let verify = eval_in(
        &mut runtime,
        crate::OperatingMode::Code,
        &canon,
        r#"
        const [r, gOld, gNew, rm] = await Promise.all([
            slim.read({ path: 'target.rs' }),
            slim.grepIndex({ queries: ['orig_token'] }),
            slim.grepIndex({ queries: ['rolled_token'] }),
            slim.readMany(['target.rs', 'other.rs']),
        ]);
        const rmAgain = await slim.readMany(['target.rs', 'other.rs']);
        return { r, gOld, gNew, rm, rmAgain, cacheLen: 1 };
        "#,
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&verify).unwrap();
    assert!(v["r"].as_str().unwrap().contains("orig_token"));
    assert!(!v["gOld"]["matches"].as_array().unwrap().is_empty());
    assert!(v["gNew"]["matches"].as_array().unwrap().is_empty());
    assert_eq!(v["rm"], v["rmAgain"]);
    assert!(runtime.codemode.session_read_cache.entries.len() >= 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validation_green_and_scratchpad_track_codemode_checks_and_mutations() {
    let workspace = Workspace::new("validation-green-scratchpad");
    std::fs::write(
        workspace.0.join("Cargo.toml"),
        "[package]\nname = \"val_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(workspace.0.join("src")).unwrap();
    std::fs::write(workspace.0.join("src/lib.rs"), "pub fn v() -> i32 {\n    1\n}\n").unwrap();

    let mut runtime = Runtime::new();
    runtime.codemode.remaining_calls = 20;
    let mut governor = CausalGovernor::default();

    // Cell 1: patch followed by green validation check -> had_validation_green == true
    let inv1 = ToolInvocation {
        batch_id: "b1",
        call_id: "c1",
        name: "codemode",
        arguments: &json!({
            "code": r#"
            await slim.patch({
                path: 'src/lib.rs',
                edits: [{ expected: '    1', replacement: '    2' }],
            });
            await slim.shell({ command: 'cargo fmt --check' });
            return { ok: true };
            "#
        })
        .to_string(),
    };
    let (res1, next_seq) = runtime
        .execute_codemode_with_governor(
            crate::OperatingMode::Code,
            &workspace.0,
            inv1,
            1,
            Some(&mut governor),
        )
        .await
        .unwrap();
    assert!(res1.success, "{}", res1.output);
    assert!(runtime.codemode.effects_for("c1").had_validation_green);
    assert!(runtime.scratchpad.touched_files.iter().any(|p| p == "src/lib.rs"));
    assert!(runtime
        .scratchpad
        .verified_commands
        .iter()
        .any(|c| c == "cargo fmt --check"));

    // Cell 2: mutation AFTER check clears had_validation_green
    let inv2 = ToolInvocation {
        batch_id: "b2",
        call_id: "c2",
        name: "codemode",
        arguments: &json!({
            "code": r#"
            await slim.shell({ command: 'cargo fmt --check' });
            await slim.patch({
                path: 'src/lib.rs',
                edits: [{ expected: '    2', replacement: '    3' }],
            });
            return { ok: true };
            "#
        })
        .to_string(),
    };
    let (res2, _) = runtime
        .execute_codemode_with_governor(
            crate::OperatingMode::Code,
            &workspace.0,
            inv2,
            next_seq,
            Some(&mut governor),
        )
        .await
        .unwrap();
    assert!(res2.success, "{}", res2.output);
    assert!(!runtime.codemode.effects_for("c2").had_validation_green);
}

#[test]
fn code_mode_deduplicates_top_level_subagent_schemas_and_frame_prose() {
    let mut runtime = Runtime::new();
    runtime.set_subagents(crate::runtime::SubagentRegistry::new());

    let code_tools = runtime.workspace_tool_definitions_for(
        crate::OperatingMode::Code,
        Path::new("."),
        false,
    );
    let code_names: Vec<_> = code_tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(!code_names.contains(&"subagent"));
    assert!(!code_names.contains(&"subagent_control"));
    let cm_desc = code_tools
        .iter()
        .find(|t| t["name"] == "codemode")
        .unwrap()["description"]
        .as_str()
        .unwrap();
    assert!(cm_desc.contains("subagent(") && cm_desc.contains("subagent_control("));

    let frame = crate::runtime::mode::ChannelFrame::new(&[], None).with_subagents(true);
    let code_text = frame.applied_text(crate::OperatingMode::Code, true);
    let auto_text = frame.applied_text(crate::OperatingMode::Auto, true);
    assert!(!code_text.contains("Ad-hoc subagents:"));
    assert!(auto_text.contains("Ad-hoc subagents:"));

    let auto_tools = runtime.workspace_tool_definitions_for(
        crate::OperatingMode::Auto,
        Path::new("."),
        false,
    );
    let auto_names: Vec<_> = auto_tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(auto_names.contains(&"subagent"));
    assert!(auto_names.contains(&"subagent_control"));
}

#[tokio::test]
async fn code_mode_enhancements_cache_survival_full_read_artifact_and_json_len() {
    // 1. Exact zero-alloc JSON length matches serde_json::to_vec including escapes & UTF-8.
    let sample = json!({
        "a": "hello \"world\"\n\t\\\x01ação",
        "b": [1, -42, true, false, null, { "k": "v" }],
        "empty": []
    });
    let expected_len = serde_json::to_vec(&sample).unwrap().len();
    assert_eq!(
        calls::json_serialized_len_bounded(&sample, expected_len),
        Some(expected_len)
    );
    assert_eq!(
        calls::json_serialized_len_bounded(&sample, expected_len - 1),
        None
    );

    // 2. Code mode reads >32 KiB (>200 lines) in full without raw:true, SessionReadCache
    // survives a disjoint patch on another file, and slim.readArtifact reads session artifacts.
    let dir = crate::runtime::temp_root::TempRoot::new("codemode-enhancements");
    let big_content: String = (1..=350)
        .map(|i| format!("line-{i:04}: {}\n", "x".repeat(110)))
        .collect();
    assert!(big_content.len() > 32 * 1024);
    std::fs::write(dir.join("big.txt"), &big_content).unwrap();
    std::fs::write(dir.join("other.txt"), "before\n").unwrap();

    let mut runtime = Runtime::new();
    let store = crate::context::ArtifactStore::new(dir.join("artifacts")).unwrap();
    let handle = store.put("test-art", b"alpha\nneedle-line\nomega\n").unwrap();
    runtime.artifact_store = Some(store);
    runtime.restored_artifact_ids.insert(handle.id.clone());

    let code = format!(
        r#"
        const full = await slim.read({{ path: "big.txt" }});
        await slim.patch({{ path: "other.txt", edits: [{{ expected: "before", replacement: "after" }}] }});
        const cached = await slim.read({{ path: "big.txt" }});
        const art = await slim.readArtifact({{ id: "{id}", pattern: "needle" }});
        return {{ has350: full.includes("line-0350"), same: full === cached, art }};
    "#,
        id = handle.id
    );
    let arguments = json!({ "code": code }).to_string();
    runtime.codemode.remaining_calls = 16;
    let mut seq = 1u64;
    let out = runtime
        .run_codemode(
            crate::OperatingMode::Code,
            &dir,
            ToolInvocation {
                batch_id: "b1",
                call_id: "c1",
                name: "codemode",
                arguments: &arguments,
            },
            &mut seq,
            Duration::from_secs(5),
        )
        .await
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["has350"], true);
    assert_eq!(parsed["same"], true);
    assert!(parsed["art"]["matches"].to_string().contains("needle-line"));
    // SessionReadCache must still hold big.txt after patching other.txt.
    assert!(!runtime.codemode.session_read_cache.is_empty());
}
