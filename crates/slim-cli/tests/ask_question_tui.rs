use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use serde_json::json;
use slim_cli::{spawn_tui_runtime, ProviderRequest, ProviderRunOptions};
use slim_core::provider::ProviderKind;
use slim_core::{OperatingMode, QuestionAnswer};
use slim_tui::api::{InteractionRequestId, UiCommand, UiEvent};

fn request(endpoint: String) -> ProviderRequest {
    ProviderRequest {
        prompt: String::new(),
        mode: OperatingMode::Auto,
        kind: ProviderKind::OpenAiCompatible,
        endpoint,
        model: "ask-question-fixture".into(),
        api_key: "offline-sentinel".into(),
        account_id: None,
        timeout: Duration::from_secs(2),
    }
}

#[test]
fn ordinary_tui_answers_question_and_provider_continues_in_the_same_run() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().expect("first accept");
        let mut first_request = [0_u8; 128 * 1024];
        let first_size = first.read(&mut first_request).expect("first request");
        let first_request = String::from_utf8_lossy(&first_request[..first_size]);
        assert!(first_request.contains("\"name\":\"ask_question\""));
        let tool_call = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "question-call-1",
                        "function": {
                            "name": "ask_question",
                            "arguments": json!({
                                "question": "Which crate should change?",
                                "options": [
                                    {"label": "core", "description": "Runtime and protocol"},
                                    {"label": "tui", "description": "Interface only"}
                                ]
                            }).to_string()
                        }
                    }]
                }
            }]
        });
        first
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {tool_call}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
                )
                .as_bytes(),
            )
            .expect("tool response");

        let (mut second, _) = listener.accept().expect("second accept");
        let mut second_request = [0_u8; 128 * 1024];
        let second_size = second.read(&mut second_request).expect("second request");
        let second_request = String::from_utf8_lossy(&second_request[..second_size]);
        assert!(second_request.contains("question-call-1"));
        assert!(second_request.contains("\\\"answer\\\":\\\"core\\\""));
        assert!(second_request.contains("\\\"source\\\":\\\"option\\\""));
        assert!(second_request.contains("\\\"option_index\\\":0"));
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"Continuing with core\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("final response");
    });

    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default(),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("change the right crate".into()))
        .expect("prompt");

    let request_id = loop {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(3))
            .expect("question event");
        if let UiEvent::QuestionRequired {
            request_id,
            question,
            options,
            persisted,
        } = event
        {
            assert!(!question.contains("offline-sentinel"));
            assert_eq!(question, "Which crate should change?");
            assert_eq!(options.len(), 2);
            assert!(!persisted);
            break request_id;
        }
    };
    assert_eq!(request_id.0.as_ref(), "run-1:question-call-1");
    let answer = QuestionAnswer::option(0, "core").expect("answer");
    channels
        .commands
        .send(UiCommand::AnswerQuestion {
            request_id: request_id.clone(),
            answer: answer.clone(),
        })
        .expect("answer");

    let mut acknowledged = false;
    let mut final_text = String::new();
    loop {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(3))
            .expect("completion event");
        match event {
            UiEvent::InteractionAcknowledged {
                request_id: acknowledged_id,
                accepted: true,
                ..
            } if acknowledged_id == request_id => acknowledged = true,
            UiEvent::AssistantDelta { text } => final_text.push_str(&text),
            UiEvent::RunCompleted { run_id: 1 } => break,
            _ => {}
        }
    }
    assert!(acknowledged);
    assert_eq!(final_text, "Continuing with core");

    channels
        .commands
        .send(UiCommand::AnswerQuestion {
            request_id: request_id.clone(),
            answer,
        })
        .expect("duplicate answer");
    let rejected = loop {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(2))
            .expect("duplicate acknowledgement");
        if let UiEvent::InteractionAcknowledged {
            request_id: rejected_id,
            accepted: false,
            ..
        } = event
        {
            break rejected_id;
        }
    };
    assert_eq!(rejected, request_id);

    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}

#[test]
fn wrong_run_question_id_is_rejected_without_reaching_the_runtime() {
    let (runtime, channels) = spawn_tui_runtime(
        request("http://127.0.0.1:9".into()),
        ProviderRunOptions::default(),
    )
    .expect("bridge");
    let request_id = InteractionRequestId("run-99:question-call-1".into());
    channels
        .commands
        .send(UiCommand::AnswerQuestion {
            request_id: request_id.clone(),
            answer: QuestionAnswer::custom("no").expect("answer"),
        })
        .expect("answer");
    assert!(matches!(
        channels
            .events_data
            .recv_timeout(Duration::from_secs(2))
            .expect("rejection"),
        UiEvent::InteractionAcknowledged {
            request_id: rejected,
            accepted: false,
            ..
        } if rejected == request_id
    ));
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
}

#[test]
fn malformed_question_fails_the_tool_without_opening_a_tui_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut first, _) = listener.accept().expect("first accept");
        let mut request = [0_u8; 128 * 1024];
        let _ = first.read(&mut request).expect("first request");
        let malformed = json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "malformed-question",
                "function": {
                    "name": "ask_question",
                    "arguments": json!({
                        "question": "Choose",
                        "options": [{"label": "only"}]
                    }).to_string()
                }
            }]}}]
        });
        first
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {malformed}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
                )
                .as_bytes(),
            )
            .expect("malformed response");

        let (mut second, _) = listener.accept().expect("second accept");
        let mut follow_up = [0_u8; 128 * 1024];
        let size = second.read(&mut follow_up).expect("follow-up request");
        let follow_up = String::from_utf8_lossy(&follow_up[..size]);
        assert!(follow_up.contains("question requires zero or 2..=5 options"));
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"Recovered\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .expect("final response");
    });
    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default(),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("ask safely".into()))
        .expect("prompt");
    let mut saw_failed_tool = false;
    loop {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(3))
            .expect("run event");
        assert!(!matches!(event, UiEvent::QuestionRequired { .. }));
        match event {
            UiEvent::ToolEnded {
                ref name,
                success: false,
                ..
            } if name == "ask_question" => saw_failed_tool = true,
            UiEvent::RunCompleted { run_id: 1 } => break,
            _ => {}
        }
    }
    assert!(saw_failed_tool);
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}

#[test]
fn cancel_run_while_question_waits_closes_without_a_follow_up_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 128 * 1024];
        let _ = stream.read(&mut request).expect("request");
        let question = json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "cancel-question",
                "function": {
                    "name": "ask_question",
                    "arguments": json!({"question": "Wait for me?"}).to_string()
                }
            }]}}]
        });
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {question}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
                )
                .as_bytes(),
            )
            .expect("question response");
    });
    let (runtime, channels) = spawn_tui_runtime(
        request(format!("http://{address}")),
        ProviderRunOptions::default(),
    )
    .expect("bridge");
    channels
        .commands
        .send(UiCommand::SendPrompt("ask then stop".into()))
        .expect("prompt");
    loop {
        let event = channels
            .events_data
            .recv_timeout(Duration::from_secs(3))
            .expect("question event");
        if matches!(event, UiEvent::QuestionRequired { .. }) {
            break;
        }
    }
    channels
        .commands
        .send(UiCommand::CancelRun)
        .expect("cancel");
    loop {
        let event = channels
            .events
            .recv_timeout(Duration::from_secs(3))
            .expect("cancel event");
        if matches!(event, UiEvent::RunCancelled { run_id: 1 }) {
            break;
        }
    }
    channels
        .commands
        .send(UiCommand::Shutdown)
        .expect("shutdown");
    drop(runtime);
    server.join().expect("server");
}
