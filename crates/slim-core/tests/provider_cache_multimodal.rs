use serde_json::json;
use slim_core::provider::{
    AnthropicAdapter, OpenAiCompatibleAdapter, ProviderAdapter, ProviderCache, ProviderConfig,
    ProviderContentBlock, ProviderEvent, ProviderMessage,
};

#[test]
fn cache_key_is_deterministic_and_excludes_credentials_and_headers() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions?api_key=fixture-secret",
        "model-a",
        "fixture-secret",
    ))
    .expect("adapter");
    let messages = [ProviderMessage::user("hello")];
    let tools_a = [json!({"name": "read", "parameters": {"path": "string"}})];
    let tools_b = [json!({"parameters": {"path": "string"}, "name": "read"})];

    let first = adapter.cache_key_with_tools(&messages, &tools_a);
    assert_eq!(first, adapter.cache_key_with_tools(&messages, &tools_b));
    assert!(!first.contains("fixture-secret"));
    assert!(!first.contains("Authorization"));
    assert!(!first.contains("x-api-key"));

    let changed_message = [ProviderMessage::user("changed")];
    assert_ne!(
        first,
        adapter.cache_key_with_tools(&changed_message, &tools_a)
    );
    let changed_tools = [json!({"name": "write"})];
    assert_ne!(
        first,
        adapter.cache_key_with_tools(&messages, &changed_tools)
    );

    let image_a = ProviderMessage::user("")
        .with_content_blocks(vec![ProviderContentBlock::image("image/png", "aGVsbG8=")]);
    let image_b = ProviderMessage::user("")
        .with_content_blocks(vec![ProviderContentBlock::image("image/png", "aGVsbG8h")]);
    assert_ne!(
        adapter.cache_key(&[image_a]),
        adapter.cache_key(&[image_b]),
        "content changes must invalidate the digest"
    );

    let same_origin_different_query = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions?api_key=other-secret",
        "model-a",
        "other-secret",
    ))
    .expect("adapter");
    assert_eq!(
        first,
        same_origin_different_query.cache_key_with_tools(&messages, &tools_a),
        "query credentials must not create a cache namespace"
    );

    let different_endpoint = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v2/chat/completions?api_key=other-secret",
        "model-a",
        "other-secret",
    ))
    .expect("adapter");
    assert_ne!(
        first,
        different_endpoint.cache_key_with_tools(&messages, &tools_a),
        "configured endpoints must remain isolated"
    );
}

#[test]
fn in_memory_cache_round_trip_and_content_invalidation_are_explicit() {
    let adapter = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "fixture-secret",
    ))
    .expect("adapter");
    let cache = ProviderCache::new();
    let messages = [ProviderMessage::user("hello")];
    let tools = [json!({"name": "read"})];
    let events = vec![ProviderEvent::TextDelta("cached".into())];

    assert!(cache.is_empty());
    cache.insert_for_adapter(&adapter, &messages, &tools, events.clone());
    assert_eq!(cache.len(), 1);
    assert_eq!(
        cache.get_for_adapter(&adapter, &messages, &tools),
        Some(events)
    );
    assert!(cache
        .get_for_adapter(&adapter, &[ProviderMessage::user("changed")], &tools)
        .is_none());
    assert!(cache.invalidate_for_adapter(&adapter, &messages, &tools));
    assert!(cache.is_empty());
}

#[test]
fn cache_rejects_tool_call_events_even_when_inserted_directly() {
    let cache = ProviderCache::new();
    cache.insert(
        "tool",
        vec![ProviderEvent::ToolCall {
            name: "read".into(),
            arguments: "{}".into(),
        }],
    );
    assert!(cache.is_empty());
}

#[test]
fn multimodal_blocks_are_offline_normalized_with_safe_fallbacks() {
    let openai = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "fixture-secret",
    ))
    .expect("adapter");
    let message = ProviderMessage::user("caption").with_content_blocks(vec![
        ProviderContentBlock::image_data_uri("data:image/png;base64,aGVsbG8=").expect("image"),
        ProviderContentBlock::Audio {
            media_type: "audio/wav".into(),
            data: "UklGRg==".into(),
        },
        ProviderContentBlock::File {
            media_type: "text/plain".into(),
            data: "ZmlsZQ==".into(),
        },
        ProviderContentBlock::Unsupported {
            kind: "remote-url".into(),
        },
    ]);
    let request = openai
        .build_messages_request_checked(&[message])
        .expect("request");
    assert!(request.body.contains("data:image/png;base64,aGVsbG8="));
    assert!(request.body.contains("input_audio"));
    assert!(request.body.contains("file content omitted"));
    assert!(request
        .body
        .contains("remote-url content unavailable offline"));
    assert!(!request.body.contains("fixture-secret"));

    let anthropic = AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://example.invalid/v1/messages",
        "claude-test",
        "fixture-secret",
    ))
    .expect("adapter");
    let image = ProviderMessage::user("")
        .with_content_blocks(vec![ProviderContentBlock::image("IMAGE/PNG", "aGVsbG8=")]);
    let request = anthropic
        .build_messages_request_checked(&[image])
        .expect("request");
    assert!(request.body.contains("\"type\":\"image\""));
    assert!(request.body.contains("\"media_type\":\"image/png\""));
}

#[test]
fn invalid_multimodal_payload_rejects_checked_requests_and_degrades_unchecked() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "fixture-secret",
    ))
    .expect("adapter");
    assert!(ProviderContentBlock::image_data_uri("https://example.invalid/image.png").is_err());
    let message =
        ProviderMessage::user("").with_content_blocks(vec![ProviderContentBlock::Image {
            media_type: "image/png".into(),
            data: "not-base64?".into(),
        }]);
    assert!(adapter
        .build_messages_request_checked(std::slice::from_ref(&message))
        .is_err());
    let request = adapter.build_messages_request(std::slice::from_ref(&message));
    assert!(request.body.contains("image content unavailable offline"));
    assert!(!request.body.contains("not-base64?"));
}

#[test]
fn multimodal_base64_requires_canonical_standard_encoding() {
    for invalid in ["aGVsbG8", "aGVsbG8=\n", "aGVsbG8-", "Zh==", "Zg="] {
        assert!(
            ProviderContentBlock::image_data_uri(format!("data:image/png;base64,{invalid}"))
                .is_err(),
            "accepted malformed base64: {invalid:?}"
        );
    }
    assert!(ProviderContentBlock::image_data_uri("data:image/png;base64,aGVsbG8=").is_ok());
}
