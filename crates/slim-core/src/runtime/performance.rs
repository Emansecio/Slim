//! Small manual fixtures for the active native dispatch and preparation paths.
use super::*;
use crate::provider::{OpenAiCodexAdapter, ProviderConfig};
use crate::session::{DurableSessionHeader, JsonlRepo, ManualRunJournal, ManualRunSpec};
use std::fs;

fn report(label: &str, samples: &mut [f64]) {
    samples.sort_by(f64::total_cmp);
    eprintln!(
        "{label}: n={} median_ms={:.3} min_ms={:.3} max_ms={:.3}",
        samples.len(),
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1]
    );
}

#[test]
#[ignore = "manual release measurement of parallel cached reads; no network"]
fn parallel_cached_read_sequence() {
    let root = std::env::temp_dir().join(format!(
        "slim-cache-perf-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let body = "source ação 日本語 line\n".repeat(200);
    let calls: Vec<_> = (0..8)
        .map(|index| {
            let path = format!("source-{index}.txt");
            fs::write(root.join(&path), &body).unwrap();
            ProviderToolCall {
                id: format!("read-{index}"),
                name: "read".into(),
                arguments: serde_json::json!({"path": path, "max_lines": 200}).to_string(),
            }
        })
        .collect();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut first = Vec::new();
    let mut warm = Vec::new();
    let mut single = Vec::new();
    for sample in 0..32 {
        let mut runtime = Runtime::new();
        let mut governor = CausalGovernor::default();
        let mut seq = 1;
        for pass in 0..2 {
            let start = Instant::now();
            let (results, next) = executor
                .block_on(runtime.execute_provider_tool_batch(
                    crate::OperatingMode::Auto,
                    &root,
                    &format!("batch-{pass}"),
                    &calls,
                    seq,
                    &mut governor,
                ))
                .unwrap();
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            seq = next;
            assert_eq!(results.len(), calls.len());
            assert!(results
                .iter()
                .all(|result| result.success && result.output == body));
            if sample > 0 {
                if pass == 0 {
                    first.push(elapsed);
                } else {
                    warm.push(elapsed);
                }
            } else {
                eprintln!("first_process_pass_{pass}_ms={elapsed:.3}");
            }
        }
        let prepared = runtime.tools.prepare_invocation(
            crate::OperatingMode::Auto,
            &root,
            "read",
            &calls[0].arguments,
        );
        let start = Instant::now();
        let outcome = runtime
            .tools
            .execute_prepared_with_cancellation_and_progress(&prepared, None, |_| {});
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        assert!(outcome.result.success);
        assert_eq!(outcome.result.output, body);
        assert_eq!(
            outcome.receipt.execution_us, 0,
            "must exercise the cache hit"
        );
        if sample > 0 {
            single.push(elapsed);
        }
    }
    report("eight_reads_first_cache_population", &mut first);
    report("eight_reads_warm_runtime_batch", &mut warm);
    report("one_prepared_cache_hit", &mut single);
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "manual release measurement; isolated files and no network"]
fn native_dispatch_and_next_request() {
    let root = std::env::temp_dir().join(format!(
        "slim-native-perf-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("source.txt"),
        "source line: ação 日本語\n".repeat(4096),
    )
    .unwrap();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor
        .block_on(async { tokio::task::spawn_blocking(|| {}).await })
        .unwrap();
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "http://127.0.0.1:1",
        "gpt-5.3-codex",
        "fixture-token",
        "fixture-account",
    ))
    .unwrap();
    let client = HttpProviderClient::new(adapter, std::time::Duration::from_secs(1)).unwrap();
    let history: Vec<_> = (0..64)
        .map(|i| {
            let text = format!("{i}: {}", "source: ação 日本語 \\\"field\\\"\n".repeat(256));
            if i % 2 == 0 {
                ProviderMessage::user(text)
            } else {
                ProviderMessage::assistant(text, vec![])
            }
        })
        .collect();
    let mut samples = vec![Vec::new(); 7];
    for i in 0..12 {
        let mut runtime = Runtime::new();
        let tools = runtime.advertised_tool_definitions(crate::OperatingMode::Auto);
        let call = ProviderToolCall {
            id: "read-1".into(),
            name: "read".into(),
            arguments: r#"{"path":"source.txt","offset":1,"max_lines":200}"#.into(),
        };
        let repo = JsonlRepo::create(
            root.join(format!("session-{i}.jsonl")),
            DurableSessionHeader::new("perf", "now", root.to_str().unwrap(), None, None),
        )
        .unwrap();
        let journal = Arc::new(Mutex::new(
            ManualRunJournal::start(
                repo,
                ManualRunSpec::new("op", "attempt", "input", "final", "inspect", 0),
            )
            .unwrap(),
        ));
        runtime.app.set_run_journal(Arc::clone(&journal));
        let mut messages = history.clone();
        let assistant = ProviderMessage::assistant("", vec![call.clone()]);
        let start = Instant::now();
        journal
            .lock()
            .unwrap()
            .begin_tools("batch", assistant.clone(), std::slice::from_ref(&call))
            .unwrap();
        let intent_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let prepared = executor
            .block_on(runtime.prepare_provider_tool_invocations(
                crate::OperatingMode::Auto,
                &root,
                std::slice::from_ref(&call),
            ))
            .unwrap();
        let preparation_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let (outcome, _) = executor
            .block_on(runtime.execute_tool_call_async(
                ToolInvocation::provider("batch", &call),
                &prepared[0],
                1,
            ))
            .unwrap();
        let dispatch_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(outcome.result.success, "{}", outcome.result.output);
        let execution_ms = outcome.receipt.execution_us as f64 / 1000.0;
        let finalization_ms = outcome.receipt.finalization_us as f64 / 1000.0;
        let start = Instant::now();
        runtime
            .append_conversation_message(&mut messages, assistant)
            .unwrap();
        runtime
            .append_conversation_message(
                &mut messages,
                ProviderMessage::tool("read", "read-1", outcome.result.output),
            )
            .unwrap();
        let upper =
            estimate_unprepared_request_chars(client.adapter(), &messages, &tools, None).unwrap();
        let request = client
            .prepare_messages_with_tools(&messages, &tools)
            .unwrap();
        assert!(upper >= request.serialized_chars());
        let next_request_ms = start.elapsed().as_secs_f64() * 1000.0;
        if i > 0 {
            for (samples, value) in samples.iter_mut().zip([
                intent_ms,
                preparation_ms,
                execution_ms,
                finalization_ms,
                dispatch_ms - execution_ms - finalization_ms,
                next_request_ms,
                intent_ms + preparation_ms + dispatch_ms + next_request_ms,
            ]) {
                samples.push(value);
            }
        }
        assert_eq!(
            outcome.receipt.bytes_read,
            200 * "source line: ação 日本語\n".len() as u64
                + "source line: ação 日本語\n".len() as u64
        );
    }
    for (label, values) in [
        "durable_intent",
        "prepare_and_schedule",
        "native_read",
        "receipt_finalization",
        "dispatch_residual_including_result_journal",
        "next_request_with_context",
        "equivalent_sequence",
    ]
    .into_iter()
    .zip(&mut samples)
    {
        report(label, values);
    }
    // All handles are scoped to the loop; the directory was created exclusively.
    fs::remove_dir_all(&root).unwrap();
}

#[test]
#[ignore = "manual release stream normalization measurement"]
fn fragmented_stream_costs() {
    for count in [1024, 8192] {
        let mut samples = Vec::new();
        for i in 0..12 {
            let mut app = AppHandle::fake();
            let mut normalizer = ProviderStreamNormalizer::new(
                ProviderKind::OpenAiCompatible,
                1,
                vec!["fixture-token".into()],
            );
            let start = Instant::now();
            for _ in 0..count {
                normalizer.push(
                    &mut app,
                    ProviderEvent::TextDelta("source ação 日本語\n".into()),
                );
            }
            normalizer.push(
                &mut app,
                ProviderEvent::Stopped {
                    reason: "stop".into(),
                },
            );
            normalizer.finish(&mut app).unwrap();
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(
                app.events()
                    .iter()
                    .filter(|event| matches!(
                        event.kind,
                        crate::EventKind::AssistantTextDelta { .. }
                    ))
                    .count(),
                count
            );
            if i > 0 {
                samples.push(elapsed);
            }
        }
        report(&format!("normalizer_fragments_{count}"), &mut samples);
    }
}

#[cfg(windows)]
#[test]
#[ignore = "manual release subprocess measurement; hidden noninteractive child"]
fn short_subprocess_costs() {
    use crate::process::{ProcessOutputBudget, ProcessRequest, ProcessRunner};
    use std::os::windows::process::CommandExt;
    let program = PathBuf::from(std::env::var_os("ComSpec").unwrap());
    let cwd = std::env::temp_dir();
    let args: Vec<std::ffi::OsString> =
        ["/d", "/c", "exit 0"].into_iter().map(Into::into).collect();
    let runner = ProcessRunner::default();
    let mut direct = Vec::new();
    let mut managed = Vec::new();
    let mut thread_snapshot = Vec::new();
    for i in 0..12 {
        // Isolate the global snapshot used to find a suspended child's initial
        // thread. This does not include thread enumeration or process creation.
        let start = Instant::now();
        unsafe {
            use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
            use windows_sys::Win32::System::Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD,
            };
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            assert_ne!(snapshot, INVALID_HANDLE_VALUE);
            assert_ne!(CloseHandle(snapshot), 0);
        }
        if i > 0 {
            thread_snapshot.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        let start = Instant::now();
        let output = std::process::Command::new(&program)
            .args(&args)
            .current_dir(&cwd)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        assert!(output.status.success());
        if i > 0 {
            direct.push(elapsed);
        }
        let start = Instant::now();
        let output = runner
            .run(ProcessRequest {
                cwd: cwd.clone(),
                program: program.clone(),
                args: args.clone(),
                timeout: std::time::Duration::from_secs(3),
                cancellation: None,
                output_budget: ProcessOutputBudget::per_stream(4096),
            })
            .unwrap();
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        assert!(output.output.status.success());
        assert!(!output.cancelled && !output.timed_out);
        if i > 0 {
            managed.push(elapsed);
        }
    }
    report("subprocess_direct_os", &mut direct);
    report("subprocess_managed", &mut managed);
    report("system_thread_snapshot_only", &mut thread_snapshot);
}

#[cfg(windows)]
#[test]
fn subprocess_then_dependent_read_uses_runtime_and_journal() {
    let root = std::env::temp_dir().join(format!(
        "slim-runner-sequence-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let mut runtime = Runtime::new();
    let calls = vec![
        ProviderToolCall {
            id: "produce".into(),
            name: "shell".into(),
            arguments: json!({"command": std::env::var("ComSpec").unwrap(),
                "args": ["/d", "/c", "echo runtime-receipt>receipt.txt"]})
            .to_string(),
        },
        ProviderToolCall {
            id: "consume".into(),
            name: "read".into(),
            arguments: json!({"path": "receipt.txt", "max_lines": 10}).to_string(),
        },
    ];
    let repo = JsonlRepo::create(
        root.join("session.jsonl"),
        DurableSessionHeader::new("perf", "now", root.to_str().unwrap(), None, None),
    )
    .unwrap();
    let journal = Arc::new(Mutex::new(
        ManualRunJournal::start(
            repo,
            ManualRunSpec::new("op", "attempt", "input", "final", "inspect", 0),
        )
        .unwrap(),
    ));
    runtime.app.set_run_journal(Arc::clone(&journal));
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let start = Instant::now();
    journal
        .lock()
        .unwrap()
        .begin_tools(
            "batch",
            ProviderMessage::assistant("", calls.clone()),
            &calls,
        )
        .unwrap();
    let (results, _) = executor
        .block_on(runtime.execute_provider_tool_batch(
            crate::OperatingMode::Auto,
            &root,
            "batch",
            &calls,
            1,
            &mut CausalGovernor::default(),
        ))
        .unwrap();
    eprintln!(
        "runtime_shell_then_read_with_journal_ms={:.3}",
        start.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| result.success), "{results:?}");
    assert!(results[1].output.contains("runtime-receipt"));
    drop(runtime);
    drop(journal);
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "manual release search-inspect-patch sequence; no network"]
fn search_context_sequence() {
    let root = std::env::temp_dir().join(format!(
        "slim-search-context-perf-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let source = format!(
        "{}fn timeout() {{\n// adjust timeout\n    return 10;\n}}\n{}",
        "// unchanged prefix line\n".repeat(4000),
        "// unchanged suffix line\n".repeat(4000)
    );
    let expected = "fn timeout() {\n// adjust timeout\n    return 10;\n}";
    let replacement = "fn timeout() {\n// adjust timeout\n    return 30;\n}";
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor
        .block_on(async { tokio::task::spawn_blocking(|| {}).await })
        .unwrap();
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "http://127.0.0.1:1",
        "gpt-5.3-codex",
        "fixture-token",
        "fixture-account",
    ))
    .unwrap();
    let client = HttpProviderClient::new(adapter, std::time::Duration::from_secs(1)).unwrap();
    let mut sequence = [Vec::new(), Vec::new(), Vec::new()];
    let mut next_context = [Vec::new(), Vec::new(), Vec::new()];
    for sample in 0..32 {
        for variant in match sample % 3 {
            0 => [0, 1, 2],
            1 => [1, 2, 0],
            _ => [2, 0, 1],
        } {
            fs::write(root.join("source.txt"), &source).unwrap();
            let mut runtime = Runtime::new();
            let tools = runtime.advertised_tool_definitions(crate::OperatingMode::Auto);
            let mut search_arguments =
                serde_json::json!({"path":"source.txt", "query":"// adjust timeout", "max_hits":1});
            if variant != 2 {
                search_arguments["context_lines"] =
                    serde_json::json!(if variant == 0 { 0 } else { 2 });
            }
            let mut calls = vec![ProviderToolCall {
                id: "locate".into(),
                name: "search".into(),
                arguments: search_arguments.to_string(),
            }];
            if variant != 1 {
                calls.push(ProviderToolCall {
                    id: "inspect".into(),
                    name: "read".into(),
                    arguments:
                        serde_json::json!({"path":"source.txt", "offset":4001, "max_lines":4})
                            .to_string(),
                });
            }
            calls.push(ProviderToolCall { id: "edit".into(), name: "patch".into(), arguments: serde_json::json!({"path":"source.txt", "edits":[{"expected":expected, "replacement":replacement}]}).to_string() });
            let mut messages = vec![ProviderMessage::user(
                "Set timeout to 30; preserve other code.",
            )];
            let mut bytes_read = 0;
            let mut output_bytes = 0;
            let mut seq = 1;
            let mut preparation_ms = 0.0;
            let start = Instant::now();
            for call in &calls {
                let prepared = executor
                    .block_on(runtime.prepare_provider_tool_invocations(
                        crate::OperatingMode::Auto,
                        &root,
                        std::slice::from_ref(call),
                    ))
                    .unwrap();
                let (outcome, next) = executor
                    .block_on(runtime.execute_tool_call_async(
                        ToolInvocation::provider("fixture", call),
                        &prepared[0],
                        seq,
                    ))
                    .unwrap();
                seq = next;
                assert!(outcome.result.success, "{}", outcome.result.output);
                if call.name == "read" {
                    assert!(outcome.result.output.contains(expected));
                }
                if call.name == "search" && variant == 1 {
                    for (line, text) in [
                        (4001, "fn timeout() {"),
                        (4003, "    return 10;"),
                        (4004, "}"),
                    ] {
                        assert!(outcome
                            .result
                            .output
                            .contains(&format!("source.txt-{line}- {text}")));
                    }
                }
                bytes_read += outcome.receipt.bytes_read;
                output_bytes += outcome.result.output.len();
                messages.push(ProviderMessage::assistant("", vec![call.clone()]));
                messages.push(ProviderMessage::tool(
                    &call.name,
                    &call.id,
                    outcome.result.output,
                ));
                let next_start = Instant::now();
                let request = client
                    .prepare_messages_with_tools(&messages, &tools)
                    .unwrap();
                assert!(request.serialized_chars() > 0);
                preparation_ms += next_start.elapsed().as_secs_f64() * 1000.0;
            }
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(
                fs::read_to_string(root.join("source.txt")).unwrap(),
                source.replace(expected, replacement)
            );
            if sample > 0 {
                sequence[variant].push(elapsed);
                next_context[variant].push(preparation_ms);
            }
            if sample == 0 {
                eprintln!("variant={variant} invocations={} bytes_read_receipts={bytes_read} output_bytes={output_bytes} source_bytes={} producer_processes=0", calls.len(), source.len());
            }
        }
    }
    report(
        "before_search_read_patch_with_request_prep",
        &mut sequence[0],
    );
    report(
        "after_context_search_patch_with_request_prep",
        &mut sequence[1],
    );
    report("before_all_next_request_preparation", &mut next_context[0]);
    report("after_all_next_request_preparation", &mut next_context[1]);
    report("omitted_context_search_read_patch", &mut sequence[2]);
    report(
        "omitted_context_all_next_request_preparation",
        &mut next_context[2],
    );
    fs::remove_dir_all(root).unwrap();
}
