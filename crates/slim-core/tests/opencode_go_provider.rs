use std::collections::HashSet;

use slim_core::provider::{
    open_code_model, open_code_models, OpenCodeApi, OpenCodeGoAdapter, ProviderAdapter,
    ProviderEvent, ProviderKind, OPENCODE_GO_BASE_URL,
};

#[test]
fn documented_registry_contains_24_unique_models() {
    let models = open_code_models();
    let unique = models.iter().map(|model| model.id).collect::<HashSet<_>>();

    assert_eq!((models.len(), unique.len()), (24, 24));
    let spark = open_code_model("muse-spark-1.3-contributor").expect("Spark 1.3 metadata");
    assert_eq!(spark.context_window, Some(1_048_576));
    assert_eq!(spark.max_output_tokens, Some(131_072));
    assert!(spark.accepts_images);
    assert_eq!(spark.reasoning_levels, &["high"]);
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
