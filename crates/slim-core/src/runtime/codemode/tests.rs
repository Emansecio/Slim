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
    let error = eval(&mut runtime, "store('too-large', 'x'.repeat(65536));")
        .await
        .unwrap_err();
    assert!(error.contains("64 KiB"), "{error}");
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
            .execute_codemode(
                mode,
                Path::new("."),
                ToolInvocation {
                    batch_id: "cell",
                    call_id: "denied",
                    name: "codemode",
                    arguments: &args,
                },
                0,
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
        native.contains("read(args: {") && native.ends_with("Promise<string>;"),
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
    for native in ["read(", "list(", "search(", "write(", "patch(", "shell("] {
        assert!(description.contains(native), "{native}: {description}");
    }
    assert!(description.contains("declare function describeTool("));
    assert!(!description.contains("code_intel("));
    assert!(!description.contains("background"));
    // The program returns the answer: no `then_run.final` on edits.
    assert!(description.contains("then_run?:"), "{description}");
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
