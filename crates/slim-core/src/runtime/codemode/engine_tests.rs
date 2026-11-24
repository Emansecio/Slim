//! The interpreter alone, against a scripted host.
use super::engine::{self, Cell, Request};
use super::*;

enum Event {
    Worker(Result<String, String>),
    Request(Option<Request>),
}

/// Runs `code` and hands every `tools.call` to `on_call`; helpers answer
/// `null`.
async fn serve(
    code: &str,
    natives: &[&str],
    timeout: Duration,
    run: CancellationToken,
    mut on_call: impl FnMut(&Cell, u32, String, Value),
) -> Result<String, String> {
    let limits = engine::Limits {
        wall: timeout,
        cpu: Duration::from_secs(120),
    };
    serve_with(
        code,
        natives,
        limits,
        run,
        |cell, ticket, name, arguments| on_call(cell, ticket, name, arguments),
    )
    .await
}

/// [`serve`] with explicit limits.
async fn serve_with(
    code: &str,
    natives: &[&str],
    limits: engine::Limits,
    run: CancellationToken,
    mut on_call: impl FnMut(&Cell, u32, String, Value),
) -> Result<String, String> {
    let mut cell = engine::spawn(
        code.into(),
        run,
        limits,
        natives.iter().map(|name| (*name).to_owned()).collect(),
    );
    loop {
        let event = tokio::select! {
            result = &mut cell.worker => Event::Worker(result.unwrap().finish(None)),
            request = cell.requests.recv() => Event::Request(request),
        };
        match event {
            Event::Worker(result) => return result,
            Event::Request(None) => return (&mut cell.worker).await.unwrap().finish(None),
            Event::Request(Some(Request::Call {
                ticket,
                name,
                arguments,
                ..
            })) => on_call(&cell, ticket, name, arguments),
            Event::Request(Some(Request::Helper { .. })) => {
                cell.reply_helper(Ok(Value::Null));
            }
            Event::Request(Some(Request::Returned)) => {}
        }
    }
}

const SECOND: Duration = Duration::from_secs(5);

#[tokio::test]
async fn promise_all_settles_calls_completed_out_of_order() {
    let mut pending = Vec::new();
    let result = serve(
        "return await Promise.all(['a', 'b', 'c'].map(id => tools.call('mcp.s.t', {id})));",
        &[],
        SECOND,
        CancellationToken::new(),
        |cell, ticket, _, arguments| {
            pending.push((ticket, arguments["id"].clone()));
            // All three are in flight before any completes; then they
            // finish in reverse order.
            if pending.len() == 3 {
                for (ticket, id) in pending.iter().rev() {
                    cell.settle(*ticket, Ok(json!({"structuredContent": {"id": id}})));
                }
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap(),
        json!([{"id": "a"}, {"id": "b"}, {"id": "c"}])
    );
}

#[tokio::test]
async fn failed_calls_reject_and_is_error_results_throw() {
    let result = serve(
        r#"
        const messages = [];
        for (const name of ['mcp.s.fail', 'mcp.s.is_error']) {
            try { await tools.call(name); } catch (error) { messages.push(error.message); }
        }
        return messages;
        "#,
        &[],
        SECOND,
        CancellationToken::new(),
        |cell, ticket, name, arguments| {
            assert_eq!(arguments, json!({}));
            if name.ends_with("fail") {
                cell.reject(ticket, "no such file");
            } else {
                cell.settle(ticket, Ok(json!({"isError": true, "content": []})));
            }
        },
    )
    .await
    .unwrap();
    let messages: Vec<String> = serde_json::from_str(&result).unwrap();
    assert_eq!(messages[0], "no such file");
    assert!(messages[1].contains("isError"), "{}", messages[1]);
}

#[tokio::test]
async fn a_promise_that_can_never_settle_is_a_deadlock() {
    let error = serve(
        "await new Promise(() => {}); return 1;",
        &[],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| unreachable!("no call was made"),
    )
    .await
    .unwrap_err();
    assert!(error.contains("deadlock"), "{error}");
}

#[tokio::test]
async fn console_is_captured_with_the_result_and_the_failure_and_truncated() {
    let output = serve(
        "console.log('one', {a: 1}); console.warn('two'); console.error(3n); return 7;",
        &[],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(output, "7\nconsole:\none {\"a\":1}\nwarn: two\nerror: 3");
    let error = serve(
        "console.info('before'); throw new Error('boom');",
        &[],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("boom") && error.ends_with("console:\nbefore"),
        "{error}"
    );
    let output = serve(
        "for (let i = 0; i < 3000; i++) console.log('é'.repeat(200)); return null;",
        &[],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap();
    let (kept, note) = output.rsplit_once('\n').unwrap();
    let kept = kept.strip_prefix("null\nconsole:\n").unwrap();
    assert!(kept.len() <= engine::MAX_CONSOLE_BYTES);
    assert!(kept.len() > engine::MAX_CONSOLE_BYTES - 2, "{}", kept.len());
    // 3000 lines of 400 bytes, less the line bytes kept.
    let dropped = 3000 * 400 - (kept.len() - kept.matches('\n').count());
    assert_eq!(note, format!("… console truncated: {dropped} more bytes"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_and_deadline_stop_a_cell_waiting_on_calls() {
    let started = Instant::now();
    let error = serve(
        "return await tools.call('mcp.s.never');",
        &[],
        Duration::from_millis(100),
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap_err();
    assert!(error.contains("cancelled or deadline exceeded"), "{error}");
    let run = CancellationToken::new();
    let cancel = run.clone();
    let error = serve(
        "return await tools.call('mcp.s.never');",
        &[],
        SECOND,
        run,
        move |_, _, _, _| cancel.cancel(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("cancelled or deadline exceeded"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn slim_binds_only_the_given_natives_and_defaults_their_arguments() {
    let mut calls = Vec::new();
    let output = serve(
        "const text = await slim.list(); return [text, Object.isFrozen(slim), Object.keys(slim)];",
        &["read", "list"],
        SECOND,
        CancellationToken::new(),
        |cell, ticket, name, arguments| {
            calls.push((name, arguments));
            cell.settle(ticket, Ok(json!("a.txt")));
        },
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!(["a.txt", true, ["read", "list"]])
    );
    assert_eq!(calls, [("slim.list".to_owned(), json!({}))]);
    let output = serve(
        "return [typeof slim, typeof __natives, typeof __log];",
        &[],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(output, r#"["undefined","undefined","undefined"]"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cpu_limit_counts_javascript_but_not_waits_for_the_host() {
    let limits = engine::Limits {
        wall: SECOND,
        cpu: Duration::from_millis(200),
    };
    let error = serve_with(
        "while (true) {}",
        &[],
        limits,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap_err();
    assert!(error.contains("CPU time limit"), "{error}");
    // A wait three times the CPU limit is not JavaScript time.
    let output = serve_with(
        "return await tools.call('mcp.s.slow');",
        &[],
        limits,
        CancellationToken::new(),
        |cell, ticket, _, _| {
            std::thread::sleep(Duration::from_millis(600));
            cell.settle(ticket, Ok(json!("done")));
        },
    )
    .await
    .unwrap();
    assert_eq!(output, "\"done\"");
}

#[tokio::test]
async fn calls_not_awaited_still_run_after_the_program_returns() {
    let mut cell = engine::spawn(
        r#"
        ['a', 'b'].forEach(async id => {
            await tools.call('mcp.s.first', {id});
            await tools.call('mcp.s.second', {id});
        });
        return 'done';
        "#
        .into(),
        CancellationToken::new(),
        engine::Limits {
            wall: SECOND,
            cpu: SECOND,
        },
        Vec::new(),
    );
    let mut seen = Vec::new();
    let output = loop {
        let event = tokio::select! {
            result = &mut cell.worker => Event::Worker(result.unwrap().finish(None)),
            request = cell.requests.recv() => Event::Request(request),
        };
        match event {
            Event::Worker(result) => break result,
            Event::Request(None) => break (&mut cell.worker).await.unwrap().finish(None),
            Event::Request(Some(Request::Call {
                ticket,
                name,
                arguments,
                ..
            })) => {
                seen.push(format!("{name} {}", arguments["id"].as_str().unwrap()));
                cell.settle(ticket, Ok(json!({})));
            }
            Event::Request(Some(Request::Returned)) => seen.push("returned".into()),
            Event::Request(Some(Request::Helper { .. })) => unreachable!(),
        }
    };
    assert_eq!(output.unwrap(), "\"done\"");
    assert_eq!(
        seen,
        [
            "mcp.s.first a",
            "mcp.s.first b",
            "returned",
            "mcp.s.second a",
            "mcp.s.second b"
        ]
    );
}

#[tokio::test]
async fn unhandled_promise_rejections_are_noted_and_handled_ones_are_cleared() {
    let output = serve(
        r#"
        const handled = Promise.reject(new Error('handled error'));
        handled.catch(() => {});
        tools.call('mcp.s.ok').then(() => { throw new Error('floating boom'); });
        await tools.call('mcp.s.ok');
        return 'done';
        "#,
        &[],
        SECOND,
        CancellationToken::new(),
        |cell, ticket, _, _| cell.settle(ticket, Ok(json!({}))),
    )
    .await
    .unwrap();
    assert!(output.starts_with("\"done\"\n"), "{output}");
    assert!(output.contains("unhandled promise rejection(s):"), "{output}");
    assert!(output.contains("floating boom"), "{output}");
    assert!(!output.contains("handled error"), "{output}");
}

#[tokio::test]
async fn prelude_normalizes_null_arguments_and_rejects_undefined_store_values() {
    let mut calls = Vec::new();
    let output = serve(
        r#"
        const res = await slim.list(null);
        return res;
        "#,
        &["list"],
        SECOND,
        CancellationToken::new(),
        |cell, ticket, name, arguments| {
            calls.push((name, arguments));
            cell.settle(ticket, Ok(json!("ok")));
        },
    )
    .await
    .unwrap();
    assert_eq!(output, "\"ok\"");
    assert_eq!(calls, [("slim.list".to_owned(), json!({}))]);

    let error = serve(
        "store('k', undefined);",
        &[],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap_err();
    assert!(error.contains("store() requires a JSON value"), "{error}");
    assert!(!error.contains("unhandled promise rejection(s):"), "{error}");
}

#[tokio::test]
async fn payload_variants_finish_helper_and_warm_runtime_isolation() {
    let mut finished = Vec::new();
    let mut cell = engine::spawn(
        r#"
        const text = await slim.read({path: 'a.txt'});
        const obj = await slim.glob({pattern: '*.rs'});
        finish('done');
        slim.finish('done-2');
        globalThis.leaked = 99;
        return {text, obj, keys: Object.keys(slim), typeofFinish: typeof finish};
        "#
        .into(),
        CancellationToken::new(),
        engine::Limits {
            wall: SECOND,
            cpu: SECOND,
        },
        vec!["read".into(), "glob".into()],
    );
    let output = loop {
        let event = tokio::select! {
            result = &mut cell.worker => Event::Worker(result.unwrap().finish(None)),
            request = cell.requests.recv() => Event::Request(request),
        };
        match event {
            Event::Worker(result) => break result.unwrap(),
            Event::Request(None) => break (&mut cell.worker).await.unwrap().finish(None).unwrap(),
            Event::Request(Some(Request::Call { ticket, name, .. })) => {
                if name == "slim.read" {
                    cell.settle(
                        ticket,
                        Ok(engine::Payload::Text("line1\n\"quoted\"\\raw".into())),
                    );
                } else {
                    cell.settle(ticket, Ok(engine::Payload::Json(r#"{"files":["a.rs"]}"#.into())));
                }
            }
            Event::Request(Some(Request::Helper { command })) => {
                if let engine::Command::Finish { answer } = command {
                    finished.push(answer);
                }
                cell.reply_helper(Ok(Value::Null));
            }
            Event::Request(Some(Request::Returned)) => {}
        }
    };
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["text"], "line1\n\"quoted\"\\raw");
    assert_eq!(parsed["obj"], json!({"files": ["a.rs"]}));
    assert_eq!(parsed["keys"], json!(["read", "glob"]));
    assert_eq!(parsed["typeofFinish"], "function");
    assert_eq!(finished, vec!["done", "done-2"]);

    // Auto mode does not bind finish(), and warm runtime preserves fresh globals.
    let next = serve(
        "return [typeof finish, typeof leaked];",
        &[],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(next, r#"["undefined","undefined"]"#);

    // After a CPU-exceeded cell, consecutive cells still execute cleanly.
    let _ = serve_with(
        "while (true) {}",
        &[],
        engine::Limits {
            wall: SECOND,
            cpu: Duration::from_millis(50),
        },
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap_err();
    let recovered = serve(
        "return 123;",
        &[],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(recovered, "123");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prelude_bytecode_prewarm_and_json_value_isolation_work() {
    assert!(
        engine::prelude_bytecode_len().is_some_and(|len| len > 100),
        "PRELUDE bytecode should compile once cleanly"
    );
    engine::prewarm_worker_cell();

    let leaked = serve(
        "globalThis.polluted = 42; return await tools.call('mcp.probe.echo', {});",
        &["read"],
        SECOND,
        CancellationToken::new(),
        |cell, ticket, _, _| {
            cell.settle(
                ticket,
                Ok(engine::Payload::JsonValue(Arc::new(json!({
                    "nested": { "arr": [1, true, "ok"] },
                })))),
            );
        },
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&leaked).unwrap(),
        json!({ "nested": { "arr": [1, true, "ok"] } })
    );

    let clean = serve(
        "return typeof globalThis.polluted;",
        &["read"],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(clean, r#""undefined""#);
}

#[test]
fn readonly_program_classifier_is_fail_closed() {
    assert!(engine::is_strictly_readonly_program(
        "const f = await slim.readMany(['a.rs', 'b.rs']); return Object.keys(f);"
    ));
    assert!(engine::is_strictly_readonly_program(
        "// write patch shell inside comment is ignored\nconst g = await slim.grepIndex('write'); return g;"
    ));
    for unsafe_prog in [
        "await slim.write({ path: 'a', content: 'x' });",
        "await slim.patch({ path: 'a', edits: [] });",
        "await slim.patchMany([]);",
        "await slim.shell({ command: 'ls' });",
        "await tools.call('mcp.s.t', {});",
        "store('k', 1); return await slim.read({ path: 'a' });",
        "const m = 'write'; await slim[m]({});",
        "finish('done'); return await slim.read({ path: 'a' });",
        "return `${await slim.write({ path: 'a', content: 'x' })}`;",
        "return 1 + 2;",
    ] {
        assert!(
            !engine::is_strictly_readonly_program(unsafe_prog),
            "must reject: {unsafe_prog}"
        );
    }
    let ts_out = engine::strip_typescript_annotations(
        "function id<T>(x: T): T { return x; }\nconst { a }: { a: number } = { a: 1 } satisfies Record<string, number>;\nreturn id(a)!.toString();",
    );
    assert!(!ts_out.contains("<T>") && !ts_out.contains("satisfies") && !ts_out.contains("!.toString"), "{ts_out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_grep_and_diff_helpers_match_contract_and_warm_runtime_survives_js_error() {
    // First trigger a JS error; the next cell must still execute cleanly on the warm runtime.
    let err = serve(
        "throw new Error('recoverable boom');",
        &["read"],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap_err();
    assert!(err.contains("recoverable boom"), "{err}");

    let out = serve(
        r#"
        const txt = "Alpha line\nbeta target\nGamma line\nBETA two\n";
        const gLiteral = grepText(txt, "beta", { ignoreCase: true, context: 1 });
        const gRegex = grepText(txt, /^Gamma/i, { context: 1 });
        const dSame = diffText("a\nb\n", "a\nb\n");
        const dChanged = diffText("a\nb\nc\n", "a\nx\nc\n", { context: 1 });
        return { gLiteral, gRegex, dSame, dChanged };
        "#,
        &["read"],
        SECOND,
        CancellationToken::new(),
        |_, _, _, _| {},
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["gLiteral"].as_array().unwrap().len(), 2);
    assert_eq!(v["gLiteral"][0]["line"], 2);
    assert_eq!(v["gLiteral"][0]["context"].as_array().unwrap().len(), 2);
    assert_eq!(v["gRegex"][0]["line"], 3);
    assert_eq!(v["dSame"], "");
    assert_eq!(v["dChanged"], "@@ -1,3 +1,3 @@\n a\n-b\n+x\n c");
}
