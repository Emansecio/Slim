use serde_json::{json, Value};
use slim_core::context::AdaptiveTokenEstimator;
use slim_core::provider::{HttpProviderClient, OpenAiCompatibleAdapter, ProviderConfig};
use slim_core::runtime::{AgentLoopConfig, AgentLoopStop};
use slim_core::{OperatingMode, Runtime};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn request(listener: &TcpListener) -> (TcpStream, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "next provider request missing");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0; 16384];
    loop {
        let size = stream.read(&mut chunk).unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&chunk[..size]);
        assert!(bytes.len() < 4 * 1024 * 1024);
        let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..end]).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        if bytes.len() >= end + 4 + length {
            return (
                stream,
                String::from_utf8(bytes[end + 4..end + 4 + length].to_vec()).unwrap(),
            );
        }
    }
}

fn respond(stream: &mut TcpStream, delta: Value, finish: &str) {
    let event = json!({"choices":[{"delta":delta,"finish_reason":finish}]});
    let body = format!("data: {event}\n\ndata: [DONE]\n\n");
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

#[test]
fn four_medium_reads_fit_the_next_request_with_coherent_pages() {
    for (window, unicode, row_count) in [
        (32_000, false, 500),
        (16_000, true, 500),
        (32_000, false, 2),
    ] {
        let root = std::env::temp_dir().join(format!(
            "slim-presentation-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let filler = if unicode {
            "界".repeat(30)
        } else {
            "x\"\\".repeat(30)
        };
        let source = (0..row_count)
            .map(|n| format!("ROW_{n:04}:{filler}\n"))
            .collect::<String>()
            + "END_OK\n";
        assert_eq!(source.len(), row_count * 100 + 7);
        for i in 0..4 {
            std::fs::write(
                root.join(format!("file{i}.txt")),
                source.replace("ROW_", &format!("R{i}W_")),
            )
            .unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut first, _) = request(&listener);
            let calls = (0..4).map(|i| json!({"index":i,"id":format!("read-{i}"),"function":{"name":"read","arguments":json!({"path":format!("file{i}.txt")}).to_string()}})).collect::<Vec<_>>();
            respond(&mut first, json!({"tool_calls":calls}), "tool_calls");
            let (mut second, wire) = request(&listener);
            respond(&mut second, json!({"content":"done"}), "stop");
            wire
        });
        let client = HttpProviderClient::new(
            OpenAiCompatibleAdapter::new(ProviderConfig::openai(endpoint, "fixture", "fixture"))
                .unwrap(),
            Duration::from_secs(10),
        )
        .unwrap();
        let mut runtime = Runtime::with_artifact_store(root.join(".slim/artifacts")).unwrap();
        let prompt = format!(
            "Inspect all four files. Preserve constraints. {}",
            "prior \\\" context\n".repeat(100)
        );
        let outcome = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(runtime.run_agent_loop(
                &client,
                &prompt,
                OperatingMode::Auto,
                &root,
                1,
                AgentLoopConfig {
                    max_turns: 2,
                    context_window_tokens: window,
                    ..AgentLoopConfig::default()
                },
            ));
        let server_result = server.join();
        let outcome = outcome.unwrap_or_else(|error| panic!("batch must reach next request (window={window}, unicode={unicode}): {error:?}; compaction={:?}; message sizes={:?}", runtime.app.events().iter().filter_map(|event| match &event.kind { slim_core::EventKind::CompactionState {tokens_before,tokens_after,..} => Some((tokens_before,tokens_after)), _ => None }).collect::<Vec<_>>(), runtime.conversation().iter().map(|message| (&message.role,message.content.len())).collect::<Vec<_>>()));
        let wire = server_result.unwrap();
        assert_eq!(outcome.stop, AgentLoopStop::ProviderCompleted);
        assert_eq!(outcome.tool_results.len(), 4);
        assert!(
            AdaptiveTokenEstimator::default().estimate(
                "fixture",
                "fixture",
                wire.chars().count() as u64
            ) + 4096
                <= window
        );
        let payload: Value = serde_json::from_str(&wire).unwrap();
        let results = payload["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 4);
        for (i, (message, raw)) in results.iter().zip(&outcome.tool_results).enumerate() {
            assert_eq!(message["tool_call_id"], format!("read-{i}"));
            let expected = source.replace("ROW_", &format!("R{i}W_"));
            assert_eq!(raw.output, expected);
            let text = message["content"].as_str().unwrap();
            if row_count == 2 {
                assert_eq!(text, expected);
                assert!(raw.artifact.is_none());
                continue;
            }
            let artifact = raw.artifact.as_ref().expect("raw artifact");
            assert_eq!(std::fs::read_to_string(&artifact.path).unwrap(), expected);
            assert!(text.contains(&format!("R{i}W_0000:")), "{text}");
            assert!(text.contains(&artifact.id));
            assert!(!text.contains("do not pass offset"));
            let lines = text
                .lines()
                .filter(|line| line.starts_with(&format!("R{i}W_")))
                .collect::<Vec<_>>();
            assert!(!lines.is_empty() && lines.len() < 500);
            for (n, line) in lines.iter().enumerate() {
                assert_eq!(*line, format!("R{i}W_{n:04}:{filler}"));
            }
            assert!(text.contains(&format!("\"offset\": {}", lines.len() + 1)));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
