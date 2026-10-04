use super::*;
use crate::session::{
    DurableSessionHeader, JsonlRepo, ManualRunJournal, ManualRunSpec, ProviderResponse,
};

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
        .run_codemode(call, &mut 0, Duration::from_millis(50))
        .await
        .unwrap();
    assert!(result.is_err());
    token.wait_for_native_work().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    let dropped = tokio::time::timeout(
        Duration::from_millis(30),
        runtime.run_codemode(call, &mut 0, CELL_TIMEOUT),
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
