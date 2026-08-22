use serde_json::json;
use slim_core::provider::{
    AnthropicAdapter, OpenAiCodexAdapter, OpenAiCompatibleAdapter, ProviderAdapter, ProviderAuth,
    ProviderConfig, ProviderContentBlock, ProviderEvent, ProviderKind, ProviderMessage,
    ProviderToolCall,
};
use slim_core::ProviderPricing;

#[test]
fn openai_compatible_request_and_stream_events_are_normalized() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    let request = adapter.build_request("hello");
    assert_eq!(request.url, "https://example.invalid/v1/chat/completions");
    assert_eq!(request.redacted_headers()[0].1, "[REDACTED]");
    assert!(request.body.contains("model-a"));
    assert!(request.body.contains("\"max_tokens\":4096"));
    assert_eq!(adapter.model(), "model-a");
    let events = adapter
        .parse_event(&json!({
            "choices": [{
                "delta": {"content": "hi", "reasoning_content": "think", "tool_calls": [{"function": {"name": "read", "arguments": "{}"}}]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2}
        }))
        .expect("events");
    assert!(events.contains(&ProviderEvent::TextDelta("hi".into())));
    assert!(events.contains(&ProviderEvent::ReasoningDelta("think".into())));
    assert!(events.contains(&ProviderEvent::ToolCallDelta {
        index: None,
        id: None,
        name: Some("read".into()),
        arguments: "{}".into(),
    }));
    assert!(events.contains(&ProviderEvent::ToolCall {
        name: "read".into(),
        arguments: "{}".into()
    }));
    assert!(events.contains(&ProviderEvent::Stopped {
        reason: "tool_calls".into()
    }));
    let messages = adapter.build_messages_request(&[
        ProviderMessage::user("inspect"),
        ProviderMessage::assistant(
            "",
            vec![ProviderToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: r#"{"path":"file.txt"}"#.into(),
            }],
        ),
        ProviderMessage::tool("read", "call-1", "1: hello"),
    ]);
    assert!(messages.body.contains("\"role\":\"tool\""));
    assert!(messages.body.contains("\"tool_call_id\":\"call-1\""));
    assert!(messages.body.contains("\"tool_calls\""));
}

#[test]
fn openai_compatible_sends_selected_reasoning_effort() {
    let adapter = OpenAiCompatibleAdapter::new(
        ProviderConfig::openai(
            "https://example.invalid/v1/chat/completions",
            "gpt-5.6-sol",
            "secret-a",
        )
        .with_reasoning_effort("xhigh"),
    )
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&adapter.build_request("hello").body).expect("request json");
    assert_eq!(body["reasoning_effort"], "xhigh");
}

#[test]
fn codex_sends_selected_reasoning_effort() {
    let adapter = OpenAiCodexAdapter::new(
        ProviderConfig::openai_codex(
            "https://chatgpt.com/backend-api",
            "gpt-5.6-terra",
            "oauth-token",
            "account-id",
        )
        .with_reasoning_effort("max"),
    )
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&adapter.build_request("hello").body).expect("request json");
    assert_eq!(body["reasoning"]["effort"], "max");
}

#[test]
fn openai_compatible_sends_native_system_prompt_by_default() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&adapter.build_request("hello").body).expect("request json");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        body["messages"][0]["content"],
        slim_core::provider::NATIVE_SYSTEM_PROMPT
    );
    assert_eq!(body["messages"][1]["role"], "user");

    let messages = adapter.build_messages_request(&[ProviderMessage::user("inspect")]);
    let body: serde_json::Value = serde_json::from_str(&messages.body).expect("request json");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], slim_core::provider::NATIVE_SYSTEM_PROMPT);
    assert_eq!(body["messages"][1]["role"], "user");
}

#[test]
fn system_prompt_override_and_disable_apply_to_openai_compatible() {
    let custom = OpenAiCompatibleAdapter::new(
        ProviderConfig::openai(
            "https://example.invalid/v1/chat/completions",
            "model-a",
            "secret-a",
        )
        .with_system_prompt("custom core"),
    )
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&custom.build_request("hello").body).expect("request json");
    assert_eq!(body["messages"][0]["content"], "custom core");

    let none = OpenAiCompatibleAdapter::new(
        ProviderConfig::openai(
            "https://example.invalid/v1/chat/completions",
            "model-a",
            "secret-a",
        )
        .without_system_prompt(),
    )
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&none.build_request("hello").body).expect("request json");
    assert!(body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .all(|m| m["role"] != "system"));
}

#[test]
fn anthropic_sends_native_system_as_top_level_field() {
    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-a",
    ))
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&adapter.build_request("hello").body).expect("request json");
    // Anthropic uses a top-level `system` field, not a message with role=system.
    assert_eq!(body["system"], slim_core::provider::NATIVE_SYSTEM_PROMPT);
    let user_messages = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == "user")
        .count();
    assert_eq!(user_messages, 1);

    let none = AnthropicAdapter::new(
        ProviderConfig::anthropic(
            "https://example.invalid/v1/messages",
            "claude-test",
            "secret-a",
        )
        .without_system_prompt(),
    )
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&none.build_request("hello").body).expect("request json");
    assert!(body.get("system").is_none());
}

#[test]
fn openai_usage_precedes_stop_when_both_share_one_payload() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    let events = adapter
        .parse_event(&json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2}
        }))
        .expect("events");
    assert!(matches!(
        events.as_slice(),
        [ProviderEvent::Usage { .. }, ProviderEvent::Stopped { .. }]
    ));
}

#[test]
fn provider_auth_debug_redacts_secrets_and_anthropic_oauth_uses_bearer() {
    let auth = ProviderAuth::OAuth {
        access_token: "oauth-secret".into(),
        account_id: None,
    };
    assert!(!format!("{auth:?}").contains("oauth-secret"));

    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic_oauth(
        "https://api.anthropic.com/v1/messages",
        "claude-test",
        "oauth-secret",
    ))
    .expect("adapter");
    let request = adapter.build_request("hello");
    assert!(request
        .headers
        .iter()
        .any(|(name, value)| name == "Authorization" && value == "Bearer oauth-secret"));
    assert!(!request
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("x-api-key")));
    assert!(request
        .headers
        .iter()
        .any(|(name, value)| { name == "anthropic-beta" && value.contains("oauth-2025-04-20") }));
}

#[test]
fn anthropic_usage_precedes_stop_when_both_share_message_delta() {
    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("adapter");
    let events = adapter
        .parse_event(&json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 4}
        }))
        .expect("events");
    assert!(matches!(
        events.as_slice(),
        [ProviderEvent::Usage { .. }, ProviderEvent::Stopped { .. }]
    ));
}

#[test]
fn anthropic_request_and_events_preserve_provider_model_without_fallback() {
    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("adapter");
    let request = adapter.build_request("hello");
    assert_eq!(adapter.kind(), ProviderKind::Anthropic);
    assert_eq!(adapter.model(), "claude-test");
    assert_eq!(request.redacted_headers()[0].1, "[REDACTED]");
    let events = adapter
        .parse_event(&json!({
            "type": "content_block_delta",
            "delta": {"text": "hello", "thinking": "reason"}
        }))
        .expect("events");
    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta("hello".into()),
            ProviderEvent::ReasoningDelta("reason".into())
        ]
    );
    let tool_events = adapter
        .parse_event(&json!({
            "type": "content_block_start",
            "index": 2,
            "content_block": {
                "type": "tool_use",
                "id": "toolu_1",
                "name": "read",
                "input": {"path": "file.txt"}
            }
        }))
        .expect("tool events");
    assert!(tool_events.contains(&ProviderEvent::ToolCallStart {
        index: 2,
        id: "toolu_1".into(),
        name: "read".into(),
    }));
    assert!(tool_events.contains(&ProviderEvent::ToolCall {
        name: "read".into(),
        arguments: r#"{"path":"file.txt"}"#.into()
    }));
    assert_eq!(
        adapter
            .parse_event(&json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}
            }))
            .expect("input delta"),
        vec![ProviderEvent::ToolCallInputDelta {
            index: 2,
            partial_json: "{\"path\":".into(),
        }]
    );
    assert_eq!(
        adapter
            .parse_event(&json!({"type": "content_block_stop", "index": 2}))
            .expect("tool stop"),
        vec![ProviderEvent::ContentBlockStop { index: 2 }]
    );
    let messages = adapter.build_messages_request(&[
        ProviderMessage::user("inspect"),
        ProviderMessage::assistant(
            "",
            vec![ProviderToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: r#"{"path":"file.txt"}"#.into(),
            }],
        ),
        ProviderMessage::tool("read", "call-1", "1: hello"),
    ]);
    assert!(messages.body.contains("tool_result"));
    assert!(messages.body.contains("tool_use"));
}

#[test]
fn codex_request_and_responses_stream_use_subscription_wire_contract() {
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://chatgpt.com/backend-api/codex/responses",
        "gpt-5.3-codex",
        "codex-secret",
        "account-1",
    ))
    .expect("adapter");
    let tool = json!({
        "name": "read",
        "description": "Read a file",
        "input_schema": {"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}
    });
    let request = adapter.build_messages_request_with_tools(
        &[ProviderMessage::user("inspect").with_content_blocks(vec![
            ProviderContentBlock::Image {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            },
        ])],
        std::slice::from_ref(&tool),
    );
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");
    assert_eq!(body["model"], "gpt-5.3-codex");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(
        body["input"][0]["content"][1]["image_url"],
        "data:image/png;base64,aGVsbG8="
    );
    assert!(request
        .headers
        .iter()
        .any(|(name, value)| name == "Authorization" && value == "Bearer codex-secret"));
    assert!(request
        .headers
        .iter()
        .any(|(name, value)| name == "chatgpt-account-id" && value == "account-1"));

    let mut events = Vec::new();
    for payload in [
        json!({"type":"response.output_text.delta","delta":"hello"}),
        json!({"type":"response.reasoning_summary_text.delta","delta":"thinking"}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call-1","name":"read","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"README.md\"}"}),
        json!({"type":"response.completed","response":{"usage":{"input_tokens":4,"output_tokens":2}}}),
    ] {
        events.extend(adapter.parse_event(&payload).expect("event"));
    }
    assert!(events.contains(&ProviderEvent::TextDelta("hello".into())));
    assert!(events.contains(&ProviderEvent::ReasoningDelta("thinking".into())));
    assert!(events.contains(&ProviderEvent::ToolCallDelta {
        index: Some(0),
        id: Some("call-1".into()),
        name: Some("read".into()),
        arguments: String::new(),
    }));
    assert!(matches!(
        events.as_slice().split_last(),
        Some((ProviderEvent::Stopped { .. }, prefix))
            if matches!(prefix.last(), Some(ProviderEvent::Usage { input_tokens: 4, output_tokens: 2 }))
    ));
}

#[test]
fn provider_tool_definitions_use_each_wire_format() {
    let tool = json!({
        "name": "read",
        "description": "Read a file",
        "input_schema": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }
    });
    let openai = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    let openai_body: serde_json::Value = serde_json::from_str(
        &openai
            .build_messages_request_with_tools(
                &[ProviderMessage::user("inspect")],
                std::slice::from_ref(&tool),
            )
            .body,
    )
    .expect("openai body");
    assert_eq!(openai_body["tools"][0]["type"], "function");
    assert_eq!(openai_body["tools"][0]["function"]["name"], "read");
    assert_eq!(
        openai_body["tools"][0]["function"]["parameters"]["required"][0],
        "path"
    );

    let anthropic = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("adapter");
    let anthropic_body: serde_json::Value = serde_json::from_str(
        &anthropic
            .build_messages_request_with_tools(&[ProviderMessage::user("inspect")], &[tool])
            .body,
    )
    .expect("anthropic body");
    assert_eq!(anthropic_body["tools"][0]["name"], "read");
    assert_eq!(anthropic_body["tools"][0]["input_schema"]["type"], "object");
}

#[test]
fn anthropic_request_marks_last_tool_with_cache_control() {
    let tool = |name: &str| {
        json!({
            "name": name,
            "description": "fixture",
            "input_schema": {"type": "object", "properties": {}}
        })
    };
    let anthropic = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("adapter");
    let body: serde_json::Value = serde_json::from_str(
        &anthropic
            .build_messages_request_with_tools(
                &[ProviderMessage::user("inspect")],
                &[tool("read"), tool("list")],
            )
            .body,
    )
    .expect("anthropic body");

    assert_eq!(body["tools"].as_array().expect("tools").len(), 2);
    assert_eq!(
        body["tools"][1]["cache_control"]["type"], "ephemeral",
        "last tool must be the cache breakpoint"
    );
    assert!(body["tools"][0].get("cache_control").is_none());
}

#[test]
fn provider_pricing_is_explicit_and_integer_based() {
    let pricing = ProviderPricing {
        input_micros_per_million: 1_500_000,
        output_micros_per_million: 3_000_000,
    };
    assert_eq!(pricing.cost_micros(1_000_000, 2_000_000), 7_500_000);
}

#[test]
fn output_cap_defaults_to_4096_and_can_be_set_explicitly() {
    let config = ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    )
    .with_max_output_tokens(123);
    assert_eq!(config.max_output_tokens(), 123);
    let adapter = OpenAiCompatibleAdapter::new(config).expect("adapter");
    assert!(adapter
        .build_request("hello")
        .body
        .contains("\"max_tokens\":123"));
}
