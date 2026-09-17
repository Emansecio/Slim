//! Adversarial coverage for `transport.rs` (RODADA 2 — Sifter).
//!
//! Drives the real `LspTransport` over an in-memory duplex pair: the fixture
//! side reads request frames and writes raw, sometimes-malformed responses.
//! Targets: JSON-RPC response dispatch, Content-Length framing edges,
//! bounded-mailbox behavior, and observable channel closure.

use std::time::Duration;

use serde_json::{json, Value};
use slim_lsp::transport::{LspTransport, NotificationReceiver, TransportError, TransportOptions};
use tokio::io::{
    AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf,
};

const MAX_BODY: usize = 64 * 1024;

struct Fixture {
    transport: LspTransport,
    notifications: NotificationReceiver,
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
}

fn options() -> TransportOptions {
    TransportOptions {
        max_message_bytes: MAX_BODY,
        request_timeout: Duration::from_secs(5),
        read_progress_timeout: Duration::from_secs(5),
        max_pending_requests: 8,
        notification_capacity: 64,
        notification_max_bytes: MAX_BODY,
    }
}

fn fixture() -> Fixture {
    fixture_with(|_, _| Err("unsupported".to_owned()))
}

fn fixture_with(
    handler: impl Fn(&str, Option<&Value>) -> Result<Value, String> + Send + Sync + 'static,
) -> Fixture {
    fixture_options(options(), Box::new(handler))
}

fn fixture_options(
    options: TransportOptions,
    handler: slim_lsp::transport::ServerRequestHandler,
) -> Fixture {
    let (client, server) = tokio::io::duplex(MAX_BODY);
    let (reader, writer) = tokio::io::split(server);
    let (transport, notifications) = LspTransport::new(Box::new(client), options, handler);
    Fixture {
        transport,
        notifications,
        reader: BufReader::new(reader),
        writer,
    }
}

/// Reads one Content-Length framed message the transport wrote to the wire.
async fn read_frame(reader: &mut BufReader<ReadHalf<DuplexStream>>) -> Option<Value> {
    let mut content_length = None;
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line).await.ok()?;
        if read == 0 {
            return None;
        }
        if line == b"\r\n" {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim_end_matches(['\r', '\n']);
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let length = content_length?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// Reads a frame with a deadline so a broken transport cannot hang the test.
async fn read_frame_bounded(reader: &mut BufReader<ReadHalf<DuplexStream>>) -> Option<Value> {
    tokio::time::timeout(Duration::from_secs(5), read_frame(reader))
        .await
        .ok()
        .flatten()
}

async fn write_frame(writer: &mut WriteHalf<DuplexStream>, body: &str) {
    let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    writer
        .write_all(frame.as_bytes())
        .await
        .expect("write frame");
    writer.flush().await.expect("flush frame");
}

async fn write_raw(writer: &mut WriteHalf<DuplexStream>, bytes: &[u8]) {
    writer.write_all(bytes).await.expect("write raw");
    writer.flush().await.expect("flush raw");
}

/// Waits until the read loop reports closed (mailbox close is the signal).
async fn wait_closed(notifications: &mut NotificationReceiver) {
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while notifications.recv().await.is_some() {}
    })
    .await;
}

/// Sends a request, then answers it with a caller-provided raw JSON body.
async fn request_with_response(
    fixture: &mut Fixture,
    method: &str,
    response_body: impl Fn(&Value) -> String,
) -> Result<Value, TransportError> {
    let (result, _) = tokio::join!(fixture.transport.request(method, json!({})), async {
        let request = read_frame_bounded(&mut fixture.reader)
            .await
            .expect("request frame on the wire");
        let body = response_body(&request);
        write_frame(&mut fixture.writer, &body).await;
    });
    result
}

// ---------------------------------------------------------------------------
// Response dispatch
// ---------------------------------------------------------------------------

/// BUG: a response carrying `"error": null` (emitted by JSON-RPC stacks that
/// serialize `Option<Error>` verbatim, e.g. several real LSP/MCP servers) is
/// dispatched to `remote_error(Value::Null)`, producing
/// `Err(Protocol("null"))` even though a valid `result` is present.
/// transport.rs:721 checks `message.get("error").is_some()` without excluding
/// the null sentinel; the provider layer already guards with
/// `!error.is_null()` (provider.rs:4495). Expected: `Ok(result)`.
/// Actual today: `Err(TransportError::Protocol("null"))`.
#[tokio::test]
async fn response_with_null_error_member_resolves_result() {
    let mut fixture = fixture();
    let result = request_with_response(&mut fixture, "initialize", |request| {
        json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "result": {"ok": true},
            "error": null,
        })
        .to_string()
    })
    .await;
    assert_eq!(
        result.expect("null error must not fail"),
        json!({"ok": true})
    );
}

#[tokio::test]
async fn response_with_error_object_becomes_remote_error() {
    let mut fixture = fixture();
    let result = request_with_response(&mut fixture, "initialize", |request| {
        json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "error": {"code": -32602, "message": "bad params", "data": {"f": 1}},
        })
        .to_string()
    })
    .await;
    match result {
        Err(TransportError::Remote {
            code,
            message,
            data,
        }) => {
            assert_eq!(code, -32602);
            assert_eq!(message, "bad params");
            assert_eq!(data, Some(json!({"f": 1})));
        }
        other => panic!("expected Remote error, got {other:?}"),
    }
}

/// Documents current behavior: a non-object, non-null `error` member (strictly
/// malformed per spec) surfaces as a Protocol error carrying the raw payload.
#[tokio::test]
async fn response_with_scalar_error_becomes_protocol_error() {
    let mut fixture = fixture();
    let result = request_with_response(&mut fixture, "initialize", |request| {
        json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "error": "boom",
        })
        .to_string()
    })
    .await;
    match result {
        Err(TransportError::Protocol(message)) => assert!(message.contains("boom")),
        other => panic!("expected Protocol error, got {other:?}"),
    }
}

/// Documents leniency: a response with neither `result` nor `error` resolves
/// with `Value::Null` instead of failing as a protocol violation.
#[tokio::test]
async fn response_with_only_id_resolves_null() {
    let mut fixture = fixture();
    let result = request_with_response(&mut fixture, "initialize", |request| {
        json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
        })
        .to_string()
    })
    .await;
    assert_eq!(result.expect("id-only response"), Value::Null);
}

/// Documents strict id typing: a response echoing the numeric id as a string
/// cannot resolve the pending request and the caller times out.
#[tokio::test]
async fn response_with_string_id_never_resolves_numeric_pending() {
    let mut fixture = fixture_options(
        TransportOptions {
            request_timeout: Duration::from_millis(300),
            ..options()
        },
        Box::new(|_, _| Err("unsupported".to_owned())),
    );
    let (result, _) = tokio::join!(fixture.transport.request("initialize", json!({})), async {
        let request = read_frame_bounded(&mut fixture.reader)
            .await
            .expect("request frame");
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        // String form of the same id: dropped by the pending map.
        let wrong = json!({"jsonrpc": "2.0", "id": id.as_i64().unwrap().to_string(), "result": {}});
        write_frame(&mut fixture.writer, &wrong.to_string()).await;
    });
    assert!(matches!(result, Err(TransportError::RequestTimeout { .. })));
}

#[tokio::test]
async fn response_for_unknown_id_is_ignored() {
    let mut fixture = fixture();
    let (result, _) = tokio::join!(fixture.transport.request("initialize", json!({})), async {
        let request = read_frame_bounded(&mut fixture.reader)
            .await
            .expect("request frame");
        // Orphaned response for an id nobody is waiting on.
        write_frame(
            &mut fixture.writer,
            &json!({"jsonrpc":"2.0","id":4096,"result":{"ghost":true}}).to_string(),
        )
        .await;
        write_frame(
            &mut fixture.writer,
            &json!({
                "jsonrpc": "2.0",
                "id": request.get("id").cloned().unwrap_or(Value::Null),
                "result": "real"
            })
            .to_string(),
        )
        .await;
    });
    assert_eq!(result.expect("response"), json!("real"));
}

#[tokio::test]
async fn out_of_order_responses_resolve_their_own_callers() {
    let mut fixture = fixture();
    let first = fixture.transport.request("slow", json!({}));
    let second = fixture.transport.request("fast", json!({}));
    let server = async {
        let req_a = read_frame_bounded(&mut fixture.reader)
            .await
            .expect("first request");
        let req_b = read_frame_bounded(&mut fixture.reader)
            .await
            .expect("second request");
        // Answer the second request first.
        write_frame(
            &mut fixture.writer,
            &json!({
                "jsonrpc": "2.0",
                "id": req_b.get("id").cloned().unwrap_or(Value::Null),
                "result": "b"
            })
            .to_string(),
        )
        .await;
        write_frame(
            &mut fixture.writer,
            &json!({
                "jsonrpc": "2.0",
                "id": req_a.get("id").cloned().unwrap_or(Value::Null),
                "result": "a"
            })
            .to_string(),
        )
        .await;
    };
    let (a, b, _) = tokio::join!(first, second, server);
    assert_eq!(a.expect("first request"), json!("a"));
    assert_eq!(b.expect("second request"), json!("b"));
}

#[tokio::test]
async fn duplicate_response_for_same_id_is_dropped() {
    let mut fixture = fixture();
    let (result, _) = tokio::join!(fixture.transport.request("initialize", json!({})), async {
        let request = read_frame_bounded(&mut fixture.reader)
            .await
            .expect("request frame");
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let body = json!({"jsonrpc":"2.0","id":id,"result":"x"}).to_string();
        write_frame(&mut fixture.writer, &body).await;
        // Replay the same response; nothing should consume it.
        write_frame(&mut fixture.writer, &body).await;
    });
    assert_eq!(result.expect("response"), json!("x"));
    // The transport survived the duplicate: still open.
    assert!(!fixture.transport.is_closed());
}

// ---------------------------------------------------------------------------
// Malformed / non-object frames
// ---------------------------------------------------------------------------

/// Documents that a syntactically valid JSON *non-object* frame (array,
/// scalar, null) is not rejected — it is delivered as a notification with an
/// empty method name.
#[tokio::test]
async fn non_object_frame_becomes_empty_method_notification() {
    let mut fixture = fixture();
    write_frame(&mut fixture.writer, "12345").await;
    let notification = tokio::time::timeout(Duration::from_secs(5), fixture.notifications.recv())
        .await
        .expect("notification")
        .expect("non-object frame is delivered");
    assert_eq!(notification.method, "");
    assert_eq!(notification.params, Value::Null);
}

#[tokio::test]
async fn response_id_null_is_ignored_not_matched() {
    let mut fixture = fixture();
    let (result, _) = tokio::join!(fixture.transport.request("initialize", json!({})), async {
        let request = read_frame_bounded(&mut fixture.reader)
            .await
            .expect("request frame");
        write_frame(
            &mut fixture.writer,
            &json!({"jsonrpc":"2.0","id":null,"result":{"wrong":true}}).to_string(),
        )
        .await;
        write_frame(
            &mut fixture.writer,
            &json!({
                "jsonrpc": "2.0",
                "id": request.get("id").cloned().unwrap_or(Value::Null),
                "result": "ok"
            })
            .to_string(),
        )
        .await;
    });
    assert_eq!(result.expect("response"), json!("ok"));
}

/// `{"id":N,"method":null}` carries an `id`, so it is dispatched as a
/// server-initiated request, not a response: our pending request keeps
/// waiting and the handler answers -32601.
#[tokio::test]
async fn id_with_null_method_is_a_server_request_not_a_response() {
    let mut fixture = fixture();
    let (result, _) = tokio::join!(fixture.transport.request("initialize", json!({})), async {
        let request = read_frame_bounded(&mut fixture.reader)
            .await
            .expect("request frame");
        let pending_id = request.get("id").cloned().unwrap_or(Value::Null);
        // Malformed: id present, method explicitly null.
        write_frame(
            &mut fixture.writer,
            &json!({"jsonrpc":"2.0","id":pending_id,"method":null,"result":{"wrong":true}})
                .to_string(),
        )
        .await;
        // The transport answers the malformed "request" on the wire.
        let reply = read_frame_bounded(&mut fixture.reader)
            .await
            .expect("transport should answer the malformed request");
        assert_eq!(reply.get("id"), Some(&pending_id));
        assert_eq!(
            reply.pointer("/error/code").and_then(Value::as_i64),
            Some(-32601)
        );
        // Our pending request was never resolved by that frame.
        write_frame(
            &mut fixture.writer,
            &json!({"jsonrpc":"2.0","id":pending_id,"result":"ok"}).to_string(),
        )
        .await;
    });
    assert_eq!(result.expect("response"), json!("ok"));
}

// ---------------------------------------------------------------------------
// Framing: Content-Length edges
// ---------------------------------------------------------------------------

/// Documents accepted odd-but-parseable Content-Length forms: leading `+`,
/// leading zeros, and surrounding whitespace all reach `usize::from_str`.
#[tokio::test]
async fn content_length_accepts_plus_leading_zeros_and_whitespace() {
    let mut fixture = fixture();
    for (header, body) in [
        ("Content-Length: +7", "{\"a\":1}"),
        ("Content-Length: 007", "{\"b\":2}"),
        ("Content-Length:  7  ", "{\"c\":3}"),
        ("Content-Length:\t7\t", "{\"d\":4}"),
    ] {
        let frame = format!("{header}\r\n\r\n{body}");
        write_raw(&mut fixture.writer, frame.as_bytes()).await;
        let notification =
            tokio::time::timeout(Duration::from_secs(5), fixture.notifications.recv())
                .await
                .expect("notification")
                .expect("frame accepted");
        assert_eq!(notification.method, "");
    }
}

#[tokio::test]
async fn content_length_with_space_inside_name_is_rejected() {
    let mut fixture = fixture();
    write_raw(&mut fixture.writer, b"Content-Length : 7\r\n\r\n{\"a\":1}").await;
    wait_closed(&mut fixture.notifications).await;
    assert!(fixture.transport.is_closed());
}

#[tokio::test]
async fn zero_length_body_is_a_fatal_parse_error() {
    let mut fixture = fixture();
    write_raw(&mut fixture.writer, b"Content-Length: 0\r\n\r\n").await;
    wait_closed(&mut fixture.notifications).await;
    assert!(fixture.transport.is_closed());
}

/// Bare-LF framing is not accepted: the header never terminates on `\n\n`,
/// so the connection is reported dead once the peer stops/EOF.
#[tokio::test]
async fn bare_lf_frame_is_not_recognized() {
    let mut fixture = fixture();
    write_raw(&mut fixture.writer, b"Content-Length: 7\n\n{\"a\":1}").await;
    // Both halves must drop: `io::split` shares the DuplexStream, so dropping
    // only the writer never delivers EOF to the transport's read side.
    drop(fixture.writer);
    drop(fixture.reader);
    wait_closed(&mut fixture.notifications).await;
    assert!(fixture.transport.is_closed());
}

#[tokio::test]
async fn oversized_content_length_closes_transport_and_fails_pending() {
    let mut fixture = fixture();
    let (result, _) = tokio::join!(fixture.transport.request("initialize", json!({})), async {
        let _ = read_frame_bounded(&mut fixture.reader).await;
        // Claim a body larger than max_message_bytes; no body needed.
        write_raw(
            &mut fixture.writer,
            format!("Content-Length: {}\r\n\r\n", MAX_BODY + 1).as_bytes(),
        )
        .await;
    });
    assert!(matches!(
        result,
        Err(TransportError::MessageTooLarge { .. })
    ));
    wait_closed(&mut fixture.notifications).await;
    assert!(fixture.transport.is_closed());
}

/// A request too large to frame fails the caller but must not tear the
/// transport down — the next request still completes.
#[tokio::test]
async fn oversized_request_is_rejected_but_transport_survives() {
    let mut fixture = fixture();
    let huge = json!({"blob": "x".repeat(MAX_BODY)});
    let result = fixture.transport.request("big", huge).await;
    assert!(matches!(
        result,
        Err(TransportError::MessageTooLarge { .. })
    ));
    assert!(!fixture.transport.is_closed());
    let ok = request_with_response(&mut fixture, "small", |request| {
        json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "result": "alive",
        })
        .to_string()
    })
    .await;
    assert_eq!(ok.expect("transport must stay usable"), json!("alive"));
}

// ---------------------------------------------------------------------------
// Server-initiated requests, notifications, lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_request_receives_method_not_found_from_handler_error() {
    let mut fixture = fixture_with(|method, _| Err(format!("no {method}")));
    write_frame(
        &mut fixture.writer,
        &json!({"jsonrpc":"2.0","id":4242,"method":"workspace/configuration","params":{}})
            .to_string(),
    )
    .await;
    let reply = read_frame_bounded(&mut fixture.reader)
        .await
        .expect("transport must answer the server request");
    assert_eq!(reply.get("id"), Some(&json!(4242)));
    assert_eq!(
        reply.pointer("/error/code").and_then(Value::as_i64),
        Some(-32601)
    );
}

#[tokio::test]
async fn server_request_ok_handler_result_is_returned() {
    let mut fixture = fixture_with(|_, _| Ok(json!({"answered": true})));
    write_frame(
        &mut fixture.writer,
        &json!({"jsonrpc":"2.0","id":7,"method":"client/registerCapability","params":{}})
            .to_string(),
    )
    .await;
    let reply = read_frame_bounded(&mut fixture.reader)
        .await
        .expect("transport must answer the server request");
    assert_eq!(reply.get("result"), Some(&json!({"answered": true})));
}

#[tokio::test]
async fn notification_flood_is_bounded_and_counted() {
    let mut fixture = fixture_options(
        TransportOptions {
            notification_capacity: 4,
            notification_max_bytes: MAX_BODY,
            ..options()
        },
        Box::new(|_, _| Err("unsupported".to_owned())),
    );
    for index in 0..20 {
        write_frame(
            &mut fixture.writer,
            &json!({"jsonrpc":"2.0","method":"window/logMessage","params":{"n":index}}).to_string(),
        )
        .await;
    }
    // Wait until the read loop has consumed the flood.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while fixture.transport.notification_drop_count() < 16 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "drops should be recorded for a bounded mailbox"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!fixture.transport.is_closed());
}

#[tokio::test]
async fn request_after_peer_eof_fails_closed_fast() {
    let mut fixture = fixture();
    drop(fixture.writer);
    drop(fixture.reader);
    wait_closed(&mut fixture.notifications).await;
    assert!(fixture.transport.is_closed());
    let result = fixture.transport.request("anything", json!({})).await;
    assert!(matches!(result, Err(TransportError::ServerClosed)));
}

#[tokio::test]
async fn zero_pending_capacity_rejects_every_request() {
    let fixture = fixture_options(
        TransportOptions {
            max_pending_requests: 0,
            ..options()
        },
        Box::new(|_, _| Err("unsupported".to_owned())),
    );
    let result = fixture.transport.request("initialize", json!({})).await;
    assert!(matches!(
        result,
        Err(TransportError::PendingCapacity { .. })
    ));
}
