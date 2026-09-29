//! Timing of tool-call events on a slow SSE stream: what the user can see
//! while the model is still writing a large call.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use slim_core::provider::{
    HttpProviderClient, OpenAiCompatibleAdapter, ProviderConfig, ProviderPhase,
};
use slim_core::runtime::CancellationToken;
use slim_core::{EventKind, Runtime, SessionEventSender};

/// Serves one SSE response: headers and `first_chunk` at once, then holds the
/// stream until `release`, then finishes the tool-call turn.
fn slow_tool_call_server(
    first_chunks: Vec<String>,
) -> (
    std::net::SocketAddr,
    mpsc::Sender<()>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 64 * 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("headers");
        for chunk in first_chunks {
            stream.write_all(chunk.as_bytes()).expect("chunk");
            stream.flush().expect("flush chunk");
        }
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
        let finish = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})
        );
        let _ = stream.write_all(finish.as_bytes());
    });
    (address, release_tx, server)
}

fn tool_chunk(arguments: &str, with_name: bool) -> String {
    let function = if with_name {
        json!({"name": "write", "arguments": arguments})
    } else {
        json!({"arguments": arguments})
    };
    let call = if with_name {
        json!({"index": 0, "id": "call_1", "type": "function", "function": function})
    } else {
        json!({"index": 0, "function": function})
    };
    format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"tool_calls":[call]}}]})
    )
}

/// Events the runtime emits while the server still holds the stream open.
fn events_before_release(
    address: std::net::SocketAddr,
    hold: Duration,
) -> Vec<(Duration, EventKind)> {
    let (event_tx, event_rx) = SessionEventSender::bounded(64, CancellationToken::new());
    let worker = thread::spawn(move || {
        let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
            format!("http://{address}"),
            "fixture-model",
            "fixture-key",
        ))
        .expect("adapter");
        let client = HttpProviderClient::new(adapter, Duration::from_secs(10)).expect("client");
        let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime");
        let mut runtime = Runtime::new();
        runtime.app.set_event_sender(event_tx);
        // The turn ends in a tool call the fixture cannot satisfy; only the
        // events before that matter here.
        let _ = tokio_runtime.block_on(runtime.run_provider(&client, "hello", 1));
    });
    let started = Instant::now();
    let mut seen = Vec::new();
    while started.elapsed() < hold {
        let remaining = hold.saturating_sub(started.elapsed());
        match event_rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(event) => seen.push((started.elapsed(), event.kind)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Dropping the receiver lets the worker finish once the server releases.
    drop(event_rx);
    let _ = worker;
    seen
}

fn preparing_details(seen: &[(Duration, EventKind)]) -> Vec<String> {
    seen.iter()
        .filter_map(|(_, kind)| match kind {
            EventKind::ProviderPhase {
                phase: ProviderPhase::PreparingTool,
                detail,
                ..
            } => detail.clone(),
            _ => None,
        })
        .collect()
}

#[test]
fn preparing_tool_phase_with_a_growing_size_is_visible_while_the_call_is_still_streaming() {
    let (address, release, server) = slow_tool_call_server(vec![
        tool_chunk("{\"path\":\"a.txt\",\"content\":\"", true),
        tool_chunk(&"a".repeat(3_000), false),
    ]);
    // The stream stays open for 1.5 s after both chunks: everything asserted
    // below was shown while the model was, as far as the user can tell, still writing.
    let seen = events_before_release(address, Duration::from_millis(1_500));
    release.send(()).ok();
    server.join().expect("server");
    assert_eq!(
        preparing_details(&seen),
        ["write · 27 B", "write · 3,0 KB"],
        "name at once, then one event per KiB of growth"
    );
    // Display-only: not one byte of the call's arguments and no tool lifecycle yet.
    let text = format!("{seen:?}");
    assert!(!text.contains("aaaaaaaa"), "argument content leaked");
    assert!(!text.contains("a.txt"), "argument content leaked");
    assert!(
        !seen
            .iter()
            .any(|(_, kind)| matches!(kind, EventKind::ToolStarted { .. })),
        "the held call must not start before the stream ends"
    );
}

#[test]
fn a_registered_secret_in_a_streaming_tool_call_is_never_shown() {
    let (address, release, server) = slow_tool_call_server(vec![
        tool_chunk("{\"path\":\"fixture-", true),
        tool_chunk("key\",\"content\":\"x\"}", false),
    ]);
    let seen = events_before_release(address, Duration::from_millis(1_000));
    release.send(()).ok();
    server.join().expect("server");
    let text = format!("{seen:?}");
    assert!(
        !text.contains("fixture-key"),
        "secret reached an event: {text}"
    );
    // Progress still flows: it is bytes and a name, not content (the model id
    // `fixture-model` legitimately appears elsewhere, so check the details alone).
    let details = preparing_details(&seen);
    assert!(!details.is_empty(), "{text}");
    assert!(
        details.iter().all(|detail| !detail.contains("fixture-")),
        "{details:?}"
    );
}
