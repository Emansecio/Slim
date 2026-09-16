use serde_json::json;
use slim_core::provider::{
    AnthropicAdapter, ClinePassAdapter, CommandCodeAdapter, HttpRequest, OpenAiCodexAdapter,
    OpenAiCompatibleAdapter, OpenCodeGoAdapter, OpenCodeZenAdapter, ProviderAdapter, ProviderAuth,
    ProviderConfig, ProviderContentBlock, ProviderError, ProviderEvent, ProviderKind,
    ProviderMessage, ProviderToolCall, UsageBreakdown, XaiAdapter, CLINEPASS_BASE_URL,
    COMMANDCODE_BASE_URL, OPENCODE_GO_BASE_URL, OPENCODE_ZEN_BASE_URL, OPENCODE_ZEN_PUBLIC_KEY,
    XAI_BASE_URL,
};
use slim_core::ProviderPricing;

#[test]
fn native_system_prompt_is_valid_utf8_text_without_mojibake() {
    let prompt = slim_core::provider::NATIVE_SYSTEM_PROMPT;
    assert!(!prompt.contains('Ã'));
    assert!(!prompt.contains('â'));
    assert!(prompt.contains('—'));
    assert!(prompt.contains('→'));
}

#[test]
fn request_redaction_covers_supported_credential_headers() {
    let request = HttpRequest {
        url: "https://example.invalid".into(),
        headers: [
            "Authorization",
            "Proxy-Authorization",
            "x-api-key",
            "api-key",
            "x-goog-api-key",
            "Cookie",
            "Set-Cookie",
            "x-auth-token",
            "x-amz-security-token",
            "x-access-token",
            "x-client-secret",
            "x-credential",
            "x-signature",
        ]
        .into_iter()
        .map(|name| (name.into(), "secret".into()))
        .collect(),
        body: String::new(),
    };

    assert!(request
        .redacted_headers()
        .iter()
        .all(|(_, value)| value == "[REDACTED]"));

    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://fixture-user:fixture-pass@example.invalid/v1/chat/completions?X-Amz-Signature=fixture-signature",
        "model-a",
        "fixture-key",
    ))
    .expect("adapter");
    let prepared = adapter
        .prepare_messages_request_checked(&[ProviderMessage::user("hello")])
        .expect("prepared request");
    let debug = format!("{prepared:?}");
    assert!(!debug.contains("fixture-user"));
    assert!(!debug.contains("fixture-pass"));
    assert!(!debug.contains("fixture-signature"));
}

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
                "delta": {"content": "hi", "reasoning_content": "think", "tool_calls": [{"index": 0, "id": "call-a", "function": {"name": "read", "arguments": "{}"}}]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2}
        }))
        .expect("events");
    assert!(events.contains(&ProviderEvent::TextDelta("hi".into())));
    assert!(events.contains(&ProviderEvent::ReasoningDelta("think".into())));
    let reasoning_at = events
        .iter()
        .position(|event| matches!(event, ProviderEvent::ReasoningDelta(_)))
        .expect("reasoning");
    let text_at = events
        .iter()
        .position(|event| matches!(event, ProviderEvent::TextDelta(_)))
        .expect("text");
    assert!(
        reasoning_at < text_at,
        "reasoning must precede visible text in the same chunk"
    );
    assert!(events.contains(&ProviderEvent::ToolCallDelta {
        index: Some(0),
        id: Some("call-a".into()),
        name: Some("read".into()),
        arguments: "{}".into(),
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::ToolCall { .. })),
        "identified OpenAI deltas must not emit an identityless mirror"
    );
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
            "terra",
            "oauth-token",
            "account-id",
        )
        .with_reasoning_effort("max"),
    )
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&adapter.build_request("hello").body).expect("request json");
    assert_eq!(body["reasoning"]["effort"], "max");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert_eq!(body["model"], "gpt-5.6-terra");
}

#[test]
fn anthropic_sends_effort_without_enabling_thinking() {
    let adapter = AnthropicAdapter::new(
        ProviderConfig::anthropic(
            "https://api.anthropic.com/v1/messages",
            "claude-sonnet-4-6",
            "fixture",
        )
        .with_reasoning_effort("medium"),
    )
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&adapter.build_request("hello").body).unwrap();
    assert_eq!(body["output_config"]["effort"], "medium");
    assert!(body.get("thinking").is_none());
}

#[test]
fn astra_preserves_reasoning_and_tools_in_normal_and_fast_requests() {
    for effort in ["low", "medium", "high", "xhigh", "max"] {
        let mut adapter = OpenAiCodexAdapter::new(
            ProviderConfig::openai_codex(
                "https://chatgpt.com/backend-api",
                "astra",
                "fixture-token",
                "fixture-account",
            )
            .with_reasoning_effort(effort),
        )
        .expect("Astra");
        let messages = [
            ProviderMessage::user("read file.txt"),
            ProviderMessage::assistant(
                "",
                vec![ProviderToolCall {
                    id: "read-1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"file.txt"}"#.into(),
                }],
            ),
            ProviderMessage::tool("read", "read-1", "1: evidence"),
        ];
        let tools = [json!({
            "name": "read",
            "description": "Read a workspace file",
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
        })];
        for fast in [true, false] {
            adapter = adapter.with_fast_mode(fast);
            let prepared = adapter
                .prepare_messages_request_with_tools_checked(&messages, &tools)
                .expect("prepared");
            let body: serde_json::Value = serde_json::from_slice(prepared.body()).expect("body");
            assert_eq!(body["model"], "gpt-6-astra");
            assert_eq!(
                body["reasoning"],
                json!({"effort": effort, "summary": "auto"})
            );
            assert_eq!(body.get("service_tier"), fast.then_some(&json!("priority")));
            assert_eq!(body["store"], false);
            assert_eq!(body["tools"][0]["name"], "read");
            assert_eq!(body["input"][2]["arguments"], r#"{"path":"file.txt"}"#);
            assert_eq!(body["input"][3]["call_id"], "read-1");
            assert_eq!(body["input"][3]["output"], "1: evidence");
            assert!(body.get("temperature").is_none());
            assert!(body.get("top_p").is_none());
        }
    }
    for effort in ["none", "minimal", "ultra", "invalid"] {
        assert!(
            OpenAiCodexAdapter::new(
                ProviderConfig::openai_codex(
                    "https://chatgpt.com/backend-api",
                    "gpt-6-astra",
                    "fixture-token",
                    "fixture-account"
                )
                .with_reasoning_effort(effort)
            )
            .is_err(),
            "unsupported Astra effort must fail before sending: {effort}"
        );
    }
}

#[test]
fn official_openai_chat_uses_native_output_and_usage_controls() {
    let adapter = OpenAiCompatibleAdapter::new(
        ProviderConfig::openai(
            "https://api.openai.com/v1/chat/completions",
            "gpt-5.6-sol",
            "fixture",
        )
        .with_max_output_tokens(123),
    )
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&adapter.build_request("hello").body).unwrap();
    assert_eq!(body["max_completion_tokens"], 123);
    assert_eq!(body["verbosity"], "low");
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(
        adapter.build_request("hello").body,
        adapter
            .build_messages_request(&[ProviderMessage::user("hello")])
            .body
    );
}

#[test]
fn responses_prompt_cache_key_tracks_only_the_stable_prefix() {
    let first = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://chatgpt.com/backend-api",
        "gpt-5.6-terra",
        "oauth-token-a",
        "account-a",
    ))
    .expect("first adapter");
    let second = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://chatgpt.com/backend-api",
        "gpt-5.6-terra",
        "oauth-token-b",
        "account-b",
    ))
    .expect("second adapter");
    let tool = json!({
        "name": "read",
        "description": "Read a file",
        "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
    });
    let first_body: serde_json::Value = serde_json::from_str(
        &first
            .build_messages_request_with_tools(
                &[ProviderMessage::user("first variable turn")],
                std::slice::from_ref(&tool),
            )
            .body,
    )
    .expect("first body");
    let second_body: serde_json::Value = serde_json::from_str(
        &second
            .build_messages_request_with_tools(
                &[ProviderMessage::user("different history and latest turn")],
                std::slice::from_ref(&tool),
            )
            .body,
    )
    .expect("second body");
    let first_key = first_body["prompt_cache_key"]
        .as_str()
        .expect("prompt cache key");
    let second_key = second_body["prompt_cache_key"]
        .as_str()
        .expect("second prompt cache key");
    assert_eq!(first_key, second_key);
    assert!(first_key.len() < 64, "provider key must remain bounded");
    assert!(first_body.get("prompt_cache_options").is_none());
    assert!(first.capabilities().supports_prompt_cache_key);
    assert!(first.capabilities().reports_cache_write_tokens);

    let changed_tool_body: serde_json::Value = serde_json::from_str(
        &first
            .build_messages_request_with_tools(
                &[ProviderMessage::user("first variable turn")],
                &[json!({"name": "list", "input_schema": {"type": "object"}})],
            )
            .body,
    )
    .expect("changed tool body");
    assert_ne!(
        first_key,
        changed_tool_body["prompt_cache_key"]
            .as_str()
            .expect("changed prompt cache key")
    );
}

#[test]
fn codex_reasoning_items_publish_lifecycle_boundaries() {
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://chatgpt.com/backend-api/codex/responses",
        "gpt-5.3-codex",
        "codex-secret",
        "account-id",
    ))
    .expect("adapter");

    assert_eq!(
        adapter
            .parse_event(&json!({
                "type": "response.output_item.added",
                "item": {"type": "reasoning", "id": "reasoning-1"}
            }))
            .expect("reasoning start"),
        vec![ProviderEvent::ReasoningStarted]
    );
    assert_eq!(
        adapter
            .parse_event(&json!({
                "type": "response.output_item.done",
                "item": {"type": "reasoning", "id": "reasoning-1"}
            }))
            .expect("reasoning end"),
        vec![ProviderEvent::ReasoningEnded]
    );
}

#[test]
fn openai_compatible_sends_native_system_and_gates_native_cache_hints() {
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
    assert!(body.get("prompt_cache_key").is_none());

    let messages = adapter.build_messages_request(&[ProviderMessage::user("inspect")]);
    let body: serde_json::Value = serde_json::from_str(&messages.body).expect("request json");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        body["messages"][0]["content"],
        slim_core::provider::NATIVE_SYSTEM_PROMPT
    );
    assert_eq!(body["messages"][1]["role"], "user");

    let official = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://api.openai.com/v1/chat/completions",
        "gpt-5.6",
        "secret-b",
    ))
    .expect("official adapter");
    let official_body: serde_json::Value =
        serde_json::from_str(&official.build_request("hello").body).expect("official body");
    assert!(official_body["prompt_cache_key"].is_string());
    assert!(official_body.get("prompt_cache_options").is_none());
    assert!(official.capabilities().supports_prompt_cache_key);
    assert!(official.capabilities().supports_prompt_cache_options);
    assert!(official.capabilities().reports_cache_write_tokens);
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
fn anthropic_sends_native_system_as_cacheable_top_level_blocks() {
    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-a",
    ))
    .expect("adapter");
    let body: serde_json::Value =
        serde_json::from_str(&adapter.build_request("hello").body).expect("request json");
    // Anthropic uses cacheable top-level system blocks, not role=system messages.
    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][0]["type"], "text");
    assert_eq!(
        body["system"][0]["text"],
        slim_core::provider::NATIVE_SYSTEM_PROMPT
    );
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    let user_messages = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == "user")
        .count();
    assert_eq!(user_messages, 1);

    let compact: serde_json::Value = serde_json::from_str(
        &adapter
            .build_compaction_request_checked(&[ProviderMessage::user("compact")])
            .expect("compaction request")
            .body,
    )
    .expect("compaction body");
    // The compaction transcript can never be read back under a different
    // system prompt: cache markers would only bill an ephemeral write.
    assert_eq!(
        compact["system"].as_str(),
        Some(slim_core::context::COMPACTION_SYSTEM_PROMPT)
    );
    assert!(compact.get("cache_control").is_none());

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
    assert_eq!(body["cache_control"]["type"], "ephemeral");
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
            "usage": {
                "prompt_tokens": 10,
                "prompt_tokens_details": {"cached_tokens": 4, "cache_write_tokens": 2},
                "completion_tokens": 6,
                "completion_tokens_details": {"reasoning_tokens": 2}
            }
        }))
        .expect("events");
    assert_eq!(
        events,
        vec![
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    uncached_input_tokens: 4,
                    cache_write_tokens: 2,
                    cache_read_tokens: 4,
                    output_tokens: 6,
                    reasoning_tokens: 2,
                    usage_unknown: false,
                },
            },
            ProviderEvent::Usage {
                input_tokens: 10,
                output_tokens: 6,
            },
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
        ]
    );
}

#[test]
fn null_or_incomplete_terminal_usage_remains_unknown() {
    let openai = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("openai");
    let events = openai
        .parse_event(&json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": null
        }))
        .expect("openai null usage");
    assert_eq!(
        events,
        vec![ProviderEvent::Stopped {
            reason: "stop".into(),
        }]
    );
    let events = openai
        .parse_event(&json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 7}
        }))
        .expect("openai partial usage");
    assert_eq!(
        events,
        vec![
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    uncached_input_tokens: 7,
                    usage_unknown: true,
                    ..UsageBreakdown::default()
                },
            },
            ProviderEvent::UsagePartial {
                input_tokens: 7,
                output_tokens: 0,
                input_complete: true,
                output_complete: false,
            },
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
        ]
    );
    let events = openai
        .parse_event(&json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {"completion_tokens": 9}
        }))
        .expect("openai output-only usage");
    assert_eq!(
        events,
        vec![
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    output_tokens: 9,
                    usage_unknown: true,
                    ..UsageBreakdown::default()
                },
            },
            ProviderEvent::UsagePartial {
                input_tokens: 0,
                output_tokens: 9,
                input_complete: false,
                output_complete: true,
            },
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
        ]
    );

    let anthropic = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("anthropic");
    let events = anthropic
        .parse_event(&json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": null
        }))
        .expect("anthropic events");
    assert_eq!(
        events,
        vec![
            ProviderEvent::Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            ProviderEvent::Stopped {
                reason: "end_turn".into(),
            },
        ]
    );

    let codex = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://chatgpt.com/backend-api/codex/responses",
        "gpt-5.3-codex",
        "codex-secret",
        "account-1",
    ))
    .expect("codex");
    let events = codex
        .parse_event(&json!({
            "type": "response.completed",
            "response": {"usage": {"input_tokens": 7}}
        }))
        .expect("codex events");
    assert_eq!(
        events,
        vec![
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    uncached_input_tokens: 7,
                    usage_unknown: true,
                    ..UsageBreakdown::default()
                },
            },
            ProviderEvent::UsagePartial {
                input_tokens: 7,
                output_tokens: 0,
                input_complete: true,
                output_complete: false,
            },
            ProviderEvent::Stopped {
                reason: "completed".into(),
            },
        ]
    );
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
fn anthropic_usage_normalizes_cache_input_and_cumulative_output() {
    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("adapter");

    let start = adapter
        .parse_event(&json!({
            "type": "message_start",
            "message": {"usage": {
                "input_tokens": 3,
                "cache_creation_input_tokens": 5,
                "cache_read_input_tokens": 7,
                "output_tokens": 1
            }}
        }))
        .expect("start");
    assert_eq!(
        start,
        vec![
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    uncached_input_tokens: 3,
                    cache_write_tokens: 5,
                    cache_read_tokens: 7,
                    output_tokens: 0,
                    reasoning_tokens: 0,
                    usage_unknown: false,
                },
            },
            ProviderEvent::UsagePartial {
                input_tokens: 15,
                output_tokens: 0,
                input_complete: true,
                output_complete: false,
            },
        ]
    );

    let end = adapter
        .parse_event(&json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 4}
        }))
        .expect("end");
    assert_eq!(
        end,
        vec![
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    uncached_input_tokens: 0,
                    cache_write_tokens: 0,
                    cache_read_tokens: 0,
                    output_tokens: 4,
                    reasoning_tokens: 0,
                    usage_unknown: false,
                },
            },
            ProviderEvent::UsagePartial {
                input_tokens: 0,
                output_tokens: 4,
                input_complete: false,
                output_complete: true,
            },
            ProviderEvent::Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            ProviderEvent::Stopped {
                reason: "end_turn".into(),
            },
        ]
    );
}

#[test]
fn anthropic_input_usage_overflow_is_rejected() {
    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret",
    ))
    .expect("adapter");
    let error = adapter
        .parse_event(&json!({
            "type": "message_start",
            "message": {"usage": {
                "input_tokens": u64::MAX,
                "cache_read_input_tokens": 1
            }}
        }))
        .expect_err("overflow must be explicit");
    assert!(matches!(
        error,
        slim_core::provider::ProviderError::InvalidResponse { .. }
    ));
}

#[test]
fn anthropic_cache_only_input_is_observed_but_not_complete() {
    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("adapter");
    assert_eq!(
        adapter
            .parse_event(&json!({
                "type": "message_start",
                "message": {"usage": {"cache_read_input_tokens": 5}}
            }))
            .expect("cache-only input"),
        vec![
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    cache_read_tokens: 5,
                    usage_unknown: true,
                    ..UsageBreakdown::default()
                },
            },
            ProviderEvent::UsagePartial {
                input_tokens: 5,
                output_tokens: 0,
                input_complete: false,
                output_complete: false,
            },
        ]
    );
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
        [
            ProviderEvent::UsageBreakdown { .. },
            ProviderEvent::UsagePartial { .. },
            ProviderEvent::Usage { .. },
            ProviderEvent::Stopped { .. }
        ]
    ));
}

#[test]
fn openai_defers_malformed_tool_fragments_until_terminal_reason() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    for payload in [
        json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "valid", "function": {"name": "read", "arguments": "{}"}},
                {"index": 1}
            ]}}]
        }),
        json!({
            "choices": [{"delta": {"tool_calls": [
                {"function": {"name": "read", "arguments": "{}"}}
            ]}}]
        }),
        json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "valid", "function": {"name": "read", "arguments": "{}"}},
                {"index": 1, "id": "wrong-type", "type": "not_function", "function": {"name": "read", "arguments": "{}"}}
            ]}}]
        }),
        json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "non-string-type", "type": 7, "function": {"name": "read", "arguments": "{}"}}
            ]}}]
        }),
    ] {
        let events = adapter
            .parse_event(&payload)
            .expect("tool fragments remain provisional before terminal reason");
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderEvent::ToolCallDelta { index, id, name, .. }
                if name.is_none() || (index.is_none() && id.is_none())
        )));
    }
}

#[test]
fn openai_accepts_tool_call_arguments_as_json_object() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    let events = adapter
        .parse_event(&json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call-x", "type": "function",
                 "function": {"name": "read", "arguments": {"path": "file.txt"}}}
            ]}}]
        }))
        .expect("object arguments are serialized");
    assert!(events.contains(&ProviderEvent::ToolCallDelta {
        index: Some(0),
        id: Some("call-x".into()),
        name: Some("read".into()),
        arguments: r#"{"path":"file.txt"}"#.into(),
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ProviderEvent::ToolCall { .. })),
        "identified OpenAI deltas must not emit an identityless mirror"
    );
}

#[test]
fn openai_identityless_tool_delta_retains_legacy_event() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    let cases = [
        (
            "identityless",
            json!({"function": {"name": "read", "arguments": "{}"}}),
            None,
            None,
            true,
        ),
        (
            "index-only",
            json!({"index": 0, "function": {"name": "read", "arguments": "{}"}}),
            Some(0),
            None,
            false,
        ),
        (
            "id-only",
            json!({"id": "call-id-only", "function": {"name": "read", "arguments": "{}"}}),
            None,
            Some("call-id-only"),
            false,
        ),
    ];
    for (label, call, index, id, emits_legacy) in cases {
        let events = adapter
            .parse_event(&json!({
                "choices": [{"delta": {"tool_calls": [call]}}]
            }))
            .expect(label);
        assert!(events.contains(&ProviderEvent::ToolCallDelta {
            index,
            id: id.map(str::to_owned),
            name: Some("read".into()),
            arguments: "{}".into(),
        }));
        assert_eq!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ToolCall { .. })),
            emits_legacy,
            "legacy mirror policy for {label}"
        );
    }
}

#[test]
fn openai_accepts_null_tool_call_arguments_as_empty_stream_fragment() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    let events = adapter
        .parse_event(&json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call-x", "type": "function",
                 "function": {"name": "read", "arguments": null}}
            ]}}]
        }))
        .expect("null arguments are an empty streaming fragment");
    assert_eq!(
        events,
        vec![ProviderEvent::ToolCallDelta {
            index: Some(0),
            id: Some("call-x".into()),
            name: Some("read".into()),
            arguments: String::new(),
        }]
    );
}

#[test]
fn openai_preserves_whitespace_content_and_skips_empty_tool_names() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    let events = adapter
        .parse_event(&json!({
            "choices": [{"delta": {
                "content": "  \n ",
                "reasoning_content": "think",
                "tool_calls": [
                    {"index": 0, "id": "call-x", "type": "function",
                     "function": {"name": "", "arguments": {"path": "a"}}}
                ]
            }}]
        }))
        .expect("whitespace content is a text delta");
    assert!(events.contains(&ProviderEvent::TextDelta("  \n ".into())));
    assert!(events.contains(&ProviderEvent::ReasoningDelta("think".into())));
    assert!(events.contains(&ProviderEvent::ToolCallDelta {
        index: Some(0),
        id: Some("call-x".into()),
        name: None,
        arguments: r#"{"path":"a"}"#.into(),
    }));
}

#[test]
fn openai_treats_null_type_as_function_and_skips_null_tool_entries() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    let events = adapter
        .parse_event(&json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call-a", "type": "function",
                 "function": {"name": "read", "arguments": ""}},
                null,
                {"index": 0, "id": null, "type": null,
                 "function": {"name": null, "arguments": "{\"path\":\"a.txt\"}"}}
            ]}}]
        }))
        .expect("null type continues the identified call");
    assert!(!events.iter().any(|event| matches!(
        event,
        ProviderEvent::ToolCallDelta { index: None, id: None, name: None, arguments }
            if arguments.is_empty()
    )),);
    assert!(events.contains(&ProviderEvent::ToolCallDelta {
        index: Some(0),
        id: Some("call-a".into()),
        name: Some("read".into()),
        arguments: String::new(),
    }));
    assert!(events.contains(&ProviderEvent::ToolCallDelta {
        index: Some(0),
        id: None,
        name: None,
        arguments: r#"{"path":"a.txt"}"#.into(),
    }));
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
            "index": 0,
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
fn anthropic_stop_reason_emits_one_terminal_usage_marker_after_partials() {
    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("adapter");
    let mut events = Vec::new();
    for payload in [
        json!({
            "type": "message_start",
            "message": {"usage": {"input_tokens": 7, "output_tokens": 0}}
        }),
        json!({
            "type": "message_delta",
            "usage": {"output_tokens": 3}
        }),
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"}
        }),
        json!({"type": "message_stop"}),
    ] {
        events.extend(adapter.parse_event(&payload).expect("Anthropic event"));
    }

    assert_eq!(
        events,
        vec![
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    uncached_input_tokens: 7,
                    ..UsageBreakdown::default()
                },
            },
            ProviderEvent::UsagePartial {
                input_tokens: 7,
                output_tokens: 0,
                input_complete: true,
                output_complete: false,
            },
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    output_tokens: 3,
                    ..UsageBreakdown::default()
                },
            },
            ProviderEvent::UsagePartial {
                input_tokens: 0,
                output_tokens: 3,
                input_complete: false,
                output_complete: true,
            },
            ProviderEvent::Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            ProviderEvent::Stopped {
                reason: "tool_use".into(),
            },
        ]
    );
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
        json!({"type":"response.completed","response":{"usage":{
            "input_tokens":10,
            "input_tokens_details":{"cached_tokens":4,"cache_write_tokens":2},
            "output_tokens":6,
            "output_tokens_details":{"reasoning_tokens":2}
        }}}),
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
    assert!(events.contains(&ProviderEvent::UsageBreakdown {
        usage: UsageBreakdown {
            uncached_input_tokens: 4,
            cache_write_tokens: 2,
            cache_read_tokens: 4,
            output_tokens: 6,
            reasoning_tokens: 2,
            usage_unknown: false,
        },
    }));
    assert!(matches!(
        events.as_slice().split_last(),
        Some((ProviderEvent::Stopped { .. }, prefix))
            if matches!(prefix.last(), Some(ProviderEvent::Usage { input_tokens: 10, output_tokens: 6 }))
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
fn checked_requests_reject_invalid_tool_arguments_across_message_and_compaction_paths() {
    let adapters: Vec<Box<dyn ProviderAdapter>> = vec![
        Box::new(
            OpenAiCompatibleAdapter::new(ProviderConfig::openai(
                "https://example.invalid/v1/chat/completions",
                "model-a",
                "secret-a",
            ))
            .expect("OpenAI adapter"),
        ),
        Box::new(
            AnthropicAdapter::new(ProviderConfig::anthropic(
                "https://example.invalid/v1/messages",
                "claude-test",
                "secret-b",
            ))
            .expect("Anthropic adapter"),
        ),
        Box::new(
            OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
                "https://example.invalid/backend-api",
                "gpt-test",
                "secret-c",
                "account-id",
            ))
            .expect("Codex adapter"),
        ),
        Box::new(
            ClinePassAdapter::new(
                CLINEPASS_BASE_URL,
                "cline-pass/future-open-model",
                "secret-d",
                None,
            )
            .expect("Cline Pass adapter"),
        ),
        Box::new(
            CommandCodeAdapter::new(
                COMMANDCODE_BASE_URL,
                "deepseek/deepseek-v4.1-flash",
                "secret-e",
                None,
            )
            .expect("Command Code adapter"),
        ),
        Box::new(
            OpenCodeGoAdapter::new(OPENCODE_GO_BASE_URL, "deepseek-v4-flash", "secret-f", None)
                .expect("OpenCode Go adapter"),
        ),
        Box::new(
            OpenCodeZenAdapter::new(
                OPENCODE_ZEN_BASE_URL,
                "big-pickle",
                OPENCODE_ZEN_PUBLIC_KEY,
                None,
            )
            .expect("OpenCode Zen adapter"),
        ),
        Box::new(XaiAdapter::new(XAI_BASE_URL, "grok-4.5", "secret-g", None).expect("xAI adapter")),
    ];
    for (arguments, expected_fragment) in [
        ("{", "invalid JSON"),
        ("null", "must be a JSON object"),
        ("[]", "must be a JSON object"),
        (r#""text""#, "must be a JSON object"),
    ] {
        let messages = [ProviderMessage::assistant(
            "",
            vec![ProviderToolCall {
                id: "fixture-call".into(),
                name: "read".into(),
                arguments: arguments.into(),
            }],
        )];
        for adapter in &adapters {
            for result in [
                adapter.prepare_messages_request_with_tools_checked(&messages, &[]),
                adapter.prepare_compaction_request_checked(&messages),
            ] {
                let Err(ProviderError::InvalidResponse { message }) = result else {
                    panic!(
                        "invalid arguments must fail before request construction: {arguments:?}"
                    );
                };
                assert!(message.contains("fixture-call"));
                assert!(message.contains("read"));
                assert!(message.contains(expected_fragment), "{message}");
            }
        }
    }
}

#[test]
fn valid_unicode_tool_arguments_round_trip_and_anthropic_unchecked_fallback_preserves_raw() {
    let arguments = r#"{"path":"á😀","metadata":{"label":"café"}}"#;
    let messages = [ProviderMessage::assistant(
        "",
        vec![ProviderToolCall {
            id: "unicode-call".into(),
            name: "read".into(),
            arguments: arguments.into(),
        }],
    )];
    let openai = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("OpenAI adapter");
    let openai_body: serde_json::Value = serde_json::from_slice(
        openai
            .prepare_messages_request_with_tools_checked(&messages, &[])
            .expect("OpenAI request")
            .body(),
    )
    .expect("OpenAI body");
    let openai_message = openai_body["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .find(|message| message["role"] == "assistant")
        })
        .expect("OpenAI assistant message");
    assert_eq!(openai_message["tool_calls"][0]["id"], "unicode-call");
    assert_eq!(openai_message["tool_calls"][0]["function"]["name"], "read");
    assert_eq!(
        openai_message["tool_calls"][0]["function"]["arguments"],
        arguments
    );

    let anthropic = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("Anthropic adapter");
    let anthropic_body: serde_json::Value = serde_json::from_slice(
        anthropic
            .prepare_messages_request_with_tools_checked(&messages, &[])
            .expect("Anthropic request")
            .body(),
    )
    .expect("Anthropic body");
    let anthropic_message = anthropic_body["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .find(|message| message["role"] == "assistant")
        })
        .expect("Anthropic assistant message");
    assert_eq!(anthropic_message["content"][0]["id"], "unicode-call");
    assert_eq!(anthropic_message["content"][0]["name"], "read");
    assert_eq!(
        anthropic_message["content"][0]["input"],
        json!({"path":"á😀","metadata":{"label":"café"}})
    );

    let invalid = [ProviderMessage::assistant(
        "",
        vec![ProviderToolCall {
            id: "raw-call".into(),
            name: "read".into(),
            arguments: "not-json".into(),
        }],
    )];
    let unchecked: serde_json::Value =
        serde_json::from_str(&anthropic.build_messages_request(&invalid).body)
            .expect("unchecked Anthropic body");
    assert_eq!(
        unchecked["messages"][0]["content"][0]["input"],
        json!("not-json")
    );
    assert!(anthropic
        .prepare_messages_request_with_tools_checked(&invalid, &[])
        .is_err());
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
    let capabilities = anthropic.capabilities();
    assert!(capabilities.supports_top_level_cache_control);
    assert!(capabilities.supports_explicit_cache_breakpoints);
    assert!(capabilities.supports_cache_ttl);
    assert!(capabilities.reports_cache_read_tokens);
    assert!(capabilities.reports_cache_write_tokens);
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
    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        body["tools"][1]["cache_control"]["type"], "ephemeral",
        "last tool must be the cache breakpoint"
    );
    assert!(body["tools"][0].get("cache_control").is_none());
}

#[test]
fn provider_usage_preserves_values_above_u32() {
    let big = u64::from(u32::MAX) + 1;
    let openai = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("openai");
    assert!(matches!(
        openai
            .parse_event(&json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": big, "completion_tokens": big}
            }))
            .expect("openai usage")
            .as_slice(),
        [ProviderEvent::UsageBreakdown { .. }, ProviderEvent::Usage { input_tokens, output_tokens }, ProviderEvent::Stopped { .. }]
            if *input_tokens == big && *output_tokens == big
    ));

    let anthropic = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("anthropic");
    assert!(matches!(
        anthropic
            .parse_event(&json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": big}}
            }))
            .expect("anthropic usage")
            .as_slice(),
        [ProviderEvent::UsageBreakdown { .. }, ProviderEvent::UsagePartial { input_tokens, .. }]
            if *input_tokens == big
    ));

    let codex = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://example.invalid/backend-api",
        "gpt-test",
        "oauth-secret",
        "account-id",
    ))
    .expect("codex");
    assert!(matches!(
        codex
            .parse_event(&json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": big, "output_tokens": big}}
            }))
            .expect("codex usage")
            .as_slice(),
        [ProviderEvent::UsageBreakdown { .. }, ProviderEvent::Usage { input_tokens, output_tokens }, ProviderEvent::Stopped { .. }]
            if *input_tokens == big && *output_tokens == big
    ));
    assert!(matches!(
        codex
            .parse_event(&json!({
                "type": "response.incomplete",
                "response": {
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": {"input_tokens": 3, "output_tokens": 4}
                }
            }))
            .expect("codex incomplete")
            .as_slice(),
        [ProviderEvent::UsageBreakdown { .. }, ProviderEvent::Usage { input_tokens: 3, output_tokens: 4 }, ProviderEvent::Stopped { reason }]
            if reason == "max_output_tokens"
    ));
    assert!(matches!(
        codex
            .parse_event(&json!({
                "type": "response.incomplete",
                "response": {
                    "usage": {"input_tokens": 1, "output_tokens": 2}
                }
            }))
            .expect("codex incomplete without details")
            .as_slice(),
        [ProviderEvent::UsageBreakdown { .. }, ProviderEvent::Usage { input_tokens: 1, output_tokens: 2 }, ProviderEvent::Stopped { reason }]
            if reason == "incomplete"
    ));
}

#[test]
fn provider_pricing_is_explicit_and_integer_based() {
    let pricing = ProviderPricing {
        input_micros_per_million: 1_500_000,
        output_micros_per_million: 3_000_000,
    };
    assert_eq!(pricing.cost_micros(1_000_000, 2_000_000), Some(7_500_000));
    assert_eq!(
        ProviderPricing {
            input_micros_per_million: u64::MAX,
            output_micros_per_million: 0,
        }
        .cost_micros(2, 0),
        Some((u128::from(u64::MAX) * 2 / 1_000_000) as u64),
        "division must happen after a wide multiplication"
    );
    assert_eq!(
        ProviderPricing {
            input_micros_per_million: 500_000,
            output_micros_per_million: 500_000,
        }
        .cost_micros(1, 1),
        Some(1),
        "component fractions must be aggregated before division"
    );
    assert_eq!(
        ProviderPricing {
            input_micros_per_million: u64::MAX,
            output_micros_per_million: u64::MAX,
        }
        .cost_micros(u64::MAX, u64::MAX),
        None,
        "unrepresentable monetary cost must stay unavailable"
    );
}

#[test]
fn malformed_or_overflowed_tool_indices_respect_each_protocol_boundary() {
    let openai = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("openai");
    let openai_events = openai
        .parse_event(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": u64::MAX,
                "id": "call",
                "function": {"name": "read", "arguments": "{}"}
            }]}}]
        }))
        .expect("OpenAI index validation is deferred until the terminal reason");
    assert_eq!(
        openai_events,
        vec![ProviderEvent::ToolCallDelta {
            index: None,
            id: None,
            name: None,
            arguments: String::new(),
        }]
    );

    let anthropic = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "secret-b",
    ))
    .expect("anthropic");
    for event in [
        json!({"type": "content_block_stop"}),
        json!({"type": "content_block_stop", "index": u64::MAX}),
    ] {
        assert_eq!(
            anthropic.parse_event(&event).expect_err("Anthropic index"),
            slim_core::provider::ProviderError::MalformedToolCall
        );
    }

    let codex = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://example.invalid/backend-api",
        "gpt-test",
        "oauth-secret",
        "account-id",
    ))
    .expect("codex");
    assert_eq!(
        codex
            .parse_event(&json!({
                "type": "response.function_call_arguments.delta",
                "output_index": u64::MAX,
                "delta": "{}"
            }))
            .expect_err("Codex index"),
        slim_core::provider::ProviderError::MalformedToolCall
    );
}

#[test]
fn malformed_codex_function_item_fails_closed() {
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://example.invalid/backend-api",
        "gpt-test",
        "oauth-secret",
        "account-id",
    ))
    .expect("codex");
    for item in [
        json!({"type": "function_call", "arguments": "{}"}),
        json!({"type": "function_call", "name": "read"}),
        json!({"type": "function_call", "name": 7, "arguments": {}}),
    ] {
        let error = adapter
            .parse_event(&json!({"type": "response.output_item.done", "item": item}))
            .expect_err("malformed function item");
        assert_eq!(error, slim_core::provider::ProviderError::MalformedToolCall);
    }
}

#[test]
fn codex_url_preserves_query_while_appending_only_to_path() {
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://example.invalid/backend-api?deployment=blue",
        "gpt-test",
        "oauth-secret",
        "account-id",
    ))
    .expect("codex");
    assert_eq!(
        adapter.build_request("hello").url,
        "https://example.invalid/backend-api/codex/responses?deployment=blue"
    );

    let complete = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://example.invalid/backend-api/codex/responses?deployment=blue",
        "gpt-test",
        "oauth-secret",
        "account-id",
    ))
    .expect("codex complete");
    assert_eq!(
        complete.build_request("hello").url,
        "https://example.invalid/backend-api/codex/responses?deployment=blue"
    );
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

    let codex = OpenAiCodexAdapter::new(
        ProviderConfig::openai_codex(
            "https://example.invalid/backend-api",
            "gpt-test",
            "oauth-secret",
            "account-id",
        )
        .with_max_output_tokens(321),
    )
    .expect("codex");
    let codex_body = serde_json::from_str::<serde_json::Value>(&codex.build_request("hello").body)
        .expect("codex body");
    assert!(codex_body.get("max_output_tokens").is_none());
}
