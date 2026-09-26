use std::collections::HashSet;

use slim_core::provider::{
    open_code_model, open_code_models, OpenCodeApi, OpenCodeGoAdapter, ProviderAdapter,
    ProviderEvent, ProviderKind, OPENCODE_GO_BASE_URL,
};

#[test]
fn responses_only_classifies_explicit_transient_codes_as_recoverable() {
    use slim_core::provider::ProviderError;
    let adapter = OpenCodeGoAdapter::new(
        OPENCODE_GO_BASE_URL,
        "muse-spark-1.3-contributor",
        "fixture",
        Some("high"),
    )
    .unwrap();
    for code in [
        "server_error",
        "overloaded_error",
        "invalid_api_key",
        "invalid_request_error",
        "insufficient_quota",
        "unknown_code",
    ] {
        for nested in [true, false] {
            let error = serde_json::json!({"code":code,"message":"server_error mentioned in arbitrary text"});
            let event = if nested {
                serde_json::json!({"type":"response.failed","response":{"error":error}})
            } else {
                serde_json::json!({"type":"error","error":error})
            };
            let parsed = adapter.parse_event(&event).unwrap_err();
            assert_eq!(
                parsed.is_explicit_transient(),
                matches!(code, "server_error" | "overloaded_error")
            );
            let ProviderError::Api { metadata, .. } = parsed else {
                panic!("Responses failure must retain structured metadata");
            };
            assert_eq!(metadata.code.as_deref(), Some(code));
            assert_eq!(metadata.status, None);
        }
    }
}

#[test]
fn session_headers_cover_all_protocols_and_auxiliary_requests() {
    use slim_core::provider::ProviderMessage;
    let messages = [ProviderMessage::user("inspect the workspace")];
    let mut conversation_header = None;
    for model in ["deepseek-v4-flash", "gpt-5.6-luna", "minimax-m3"] {
        let adapter = OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, model, "test-key", None)
            .unwrap()
            .with_session_id("conversation-one");
        let requests = [
            adapter.build_request("inspect"),
            adapter.build_messages_request(&messages),
            adapter
                .build_messages_request_with_tools_checked(&messages, &[])
                .unwrap(),
        ];
        let prepared = adapter
            .prepare_messages_request_with_tools_checked(&messages, &[])
            .unwrap();
        let compact = adapter
            .prepare_compaction_request_checked(&messages)
            .unwrap();
        for headers in requests
            .iter()
            .map(|r| r.headers.as_slice())
            .chain([prepared.headers(), compact.headers()])
        {
            let session: Vec<_> = headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("x-opencode-session"))
                .collect();
            assert_eq!(session.len(), 1);
            assert!(!session[0].1.is_empty());
            assert_eq!(
                conversation_header.get_or_insert_with(|| session[0].1.clone()),
                &session[0].1
            );
            assert!(headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("user-agent") && v.starts_with("slim/")));
        }
        let other = OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, model, "test-key", None)
            .unwrap()
            .with_session_id("conversation-two")
            .build_request("inspect");
        assert!(!other
            .headers
            .iter()
            .any(|(k, v)| k == "x-opencode-session" && Some(v) == conversation_header.as_ref()));
    }
}

#[test]
fn documented_registry_contains_25_unique_models() {
    let models = open_code_models();
    let unique = models.iter().map(|model| model.id).collect::<HashSet<_>>();

    assert_eq!((models.len(), unique.len()), (25, 25));
    let spark = open_code_model("muse-spark-1.3-contributor").expect("Spark 1.3 metadata");
    assert_eq!(spark.context_window, Some(1_048_576));
    assert_eq!(spark.max_output_tokens, Some(131_072));
    assert!(spark.accepts_images);
    assert_eq!(spark.reasoning_levels, &["low", "medium", "high", "xhigh"]);
}

#[test]
fn deepseek_v4_1_flash_uses_documented_chat_metadata() {
    let model = open_code_model("deepseek-flash").expect("DeepSeek V4.1 Flash metadata");

    assert_eq!(model.name, "DeepSeek V4.1 Flash");
    assert_eq!(model.api, OpenCodeApi::ChatCompletions);
    assert_eq!(model.context_window, Some(1_000_000));
    assert_eq!(model.max_output_tokens, Some(384_000));
    assert!(model.accepts_images);
    assert_eq!(model.reasoning_levels, &["low", "high", "max"]);
    assert_eq!(
        open_code_model("deepseek-v4.1-flash").map(|model| model.id),
        Some("deepseek-flash")
    );
}

#[test]
fn deepseek_v4_1_flash_entry_uses_canonical_wire_id() {
    let adapter = OpenCodeGoAdapter::new(
        OPENCODE_GO_BASE_URL,
        "deepseek-v4.1-flash",
        "go-secret",
        None,
    )
    .expect("entry adapter");

    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");

    assert_eq!(body["model"], "deepseek-flash");
}

#[test]
fn registry_exposes_context_output_reasoning_and_image_metadata() {
    for model in open_code_models() {
        assert!(model.context_window.is_some_and(|tokens| tokens > 0));
        assert!(model.max_output_tokens.is_some_and(|tokens| tokens > 0));
        assert!(!model.reasoning_levels.is_empty());
    }
    let luna = open_code_model("gpt-5.6-luna").expect("Luna metadata");
    assert_eq!(luna.context_window, Some(1_050_000));
    assert_eq!(luna.max_output_tokens, Some(128_000));
    assert!(luna.accepts_images);
    assert_eq!(
        luna.reasoning_levels,
        &["low", "medium", "high", "xhigh", "max"]
    );
    assert!(
        !open_code_model("deepseek-v4-flash")
            .expect("DeepSeek metadata")
            .accepts_images
    );
}

#[test]
fn default_model_uses_chat_completions() {
    let model = open_code_model("deepseek-v4-flash").expect("documented default");

    assert_eq!(model.api, OpenCodeApi::ChatCompletions);
}

#[test]
fn responses_model_uses_documented_protocol() {
    assert_eq!(
        open_code_model("gpt-5.6-luna").map(|model| model.api),
        Some(OpenCodeApi::Responses)
    );
}

#[test]
fn messages_model_uses_documented_protocol() {
    assert_eq!(
        open_code_model("minimax-m3").map(|model| model.api),
        Some(OpenCodeApi::AnthropicMessages)
    );
}

#[test]
fn endpoint_only_unknown_model_is_not_supported() {
    assert!(open_code_model("kimi-k2.5").is_none());
}

#[test]
fn logical_provider_is_distinct_from_wire_protocol() {
    assert_ne!(ProviderKind::OpenCodeGo, ProviderKind::OpenAiCompatible);
}

#[test]
fn chat_model_uses_bearer_chat_completions() {
    let adapter = OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, "glm-5.3", "go-secret", None)
        .expect("adapter")
        .with_system_prompt("skill-marker");

    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");

    assert!(request.url.ends_with("/v1/chat/completions"));
    assert!(request
        .headers
        .iter()
        .any(|(name, value)| name == "Authorization" && value == "Bearer go-secret"));
    assert_eq!(body["messages"][0]["content"], "skill-marker");
}

#[test]
fn deepseek_v4_1_flash_enables_chat_thinking_with_documented_effort() {
    let adapter = OpenCodeGoAdapter::new(
        OPENCODE_GO_BASE_URL,
        "deepseek-flash",
        "go-secret",
        Some("low"),
    )
    .expect("adapter");

    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");

    assert!(request.url.ends_with("/v1/chat/completions"));
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["thinking"], serde_json::json!({"type": "enabled"}));
    assert!(OpenCodeGoAdapter::new(
        OPENCODE_GO_BASE_URL,
        "deepseek-flash",
        "go-secret",
        Some("medium")
    )
    .is_err());
}

#[test]
fn responses_model_omits_codex_subscription_headers() {
    let adapter = OpenCodeGoAdapter::new(
        OPENCODE_GO_BASE_URL,
        "gpt-5.6-luna",
        "go-secret",
        Some("high"),
    )
    .expect("adapter")
    .with_max_output_tokens(321)
    .with_system_prompt("skill-marker");

    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");

    assert!(request.url.ends_with("/v1/responses"));
    assert_eq!(body["max_output_tokens"], 321);
    assert_eq!(body["instructions"], "skill-marker");
    assert!(adapter
        .sensitive_values()
        .iter()
        .any(|value| value == "go-secret"));
    assert!(!request.headers.iter().any(|(name, _)| matches!(
        name.as_str(),
        "chatgpt-account-id" | "OpenAI-Beta" | "originator"
    )));
}

#[test]
fn messages_model_uses_bearer_without_claude_oauth_headers() {
    let adapter = OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, "minimax-m3", "go-secret", None)
        .expect("adapter")
        .with_system_prompt("skill-marker");

    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");

    assert!(request.url.ends_with("/v1/messages"));
    assert!(request
        .headers
        .iter()
        .any(|(name, value)| name == "Authorization" && value == "Bearer go-secret"));
    assert!(!request
        .headers
        .iter()
        .any(|(name, _)| matches!(name.as_str(), "x-api-key" | "anthropic-beta" | "x-app")));
    assert_eq!(body["system"][0]["text"], "skill-marker");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["cache_control"]["type"], "ephemeral");
}

#[test]
fn responses_events_use_common_provider_events() {
    let adapter = OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, "gpt-5.6-luna", "go-secret", None)
        .expect("adapter");

    let events = adapter
        .parse_event(&serde_json::json!({
            "type": "response.completed",
            "response": {"usage": {"input_tokens": 3, "output_tokens": 2}}
        }))
        .expect("events");

    assert!(matches!(
        events.as_slice(),
        [
            ProviderEvent::UsageBreakdown { usage },
            ProviderEvent::Usage { input_tokens: 3, output_tokens: 2 },
            ProviderEvent::Stopped { reason }
        ] if usage.uncached_input_tokens == 3
            && usage.cache_write_tokens == 0
            && usage.cache_read_tokens == 0
            && usage.output_tokens == 2
            && usage.reasoning_tokens == 0
            && !usage.usage_unknown
            && reason == "completed"
    ));
}

#[test]
fn muse_models_use_responses_for_tools_history_and_compaction() {
    use serde_json::{json, Value};
    use slim_core::provider::{ProviderMessage, ProviderToolCall};

    for model in ["muse-spark-1.2-contributor", "muse-spark-1.3-contributor"] {
        let adapter = OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, model, "fixture", Some("xhigh"))
            .unwrap()
            .with_max_output_tokens(321);
        let history = [
            ProviderMessage::user("Read file.txt"),
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"file.txt"}"#.into(),
                }],
            ),
            ProviderMessage::tool("read", "call-1", "file content"),
        ];
        let tools = [
            json!({"name":"read", "description":"Read a file", "input_schema":{
                "type":"object", "properties":{"path":{"type":"string"}}, "required":["path"]
            }}),
        ];
        let request = adapter
            .prepare_messages_request_with_tools_checked(&history, &tools)
            .unwrap();
        assert_eq!(
            request.url(),
            format!("{OPENCODE_GO_BASE_URL}/responses"),
            "{model}"
        );
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        assert_eq!(body["model"], model);
        assert_eq!(body["max_output_tokens"], 321);
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read");
        assert!(body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output"
                && item["call_id"] == "call-1"
                && item["output"] == "file content"));
        let compact = adapter
            .prepare_compaction_request_checked(&history)
            .unwrap();
        assert_eq!(compact.url(), request.url());
        let compact_body: Value = serde_json::from_slice(compact.body()).unwrap();
        assert!(compact_body["input"].is_array());
        assert_eq!(compact_body["reasoning"]["effort"], "xhigh");
        assert!(
            OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, model, "fixture", Some("max")).is_err()
        );
        let events = adapter
            .parse_event(&json!({
                "type":"response.output_text.delta", "delta":"done"
            }))
            .unwrap();
        assert!(matches!(events.as_slice(), [ProviderEvent::TextDelta(text)] if text == "done"));
    }
}

#[test]
fn model_contract_applies_to_prepared_history_and_compaction() {
    use slim_core::provider::{ProviderContentBlock, ProviderMessage};
    assert!(OpenCodeGoAdapter::new(
        OPENCODE_GO_BASE_URL,
        "deepseek-v4-flash",
        "fixture",
        Some("low")
    )
    .is_err());
    let adapter = OpenCodeGoAdapter::new(
        OPENCODE_GO_BASE_URL,
        "deepseek-v4-flash",
        "fixture",
        Some("high"),
    )
    .unwrap();
    let history = [
        ProviderMessage::user("old image")
            .with_content_blocks(vec![ProviderContentBlock::image("image/png", "aGk=")]),
        ProviderMessage::user("continue"),
    ];
    assert!(adapter.prepare_messages_request_checked(&history).is_err());
    let compact = adapter
        .prepare_compaction_request_checked(&[ProviderMessage::user("summary")])
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(compact.body()).unwrap();
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["thinking"], serde_json::json!({"type":"enabled"}));
}
