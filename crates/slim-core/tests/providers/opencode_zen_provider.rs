use std::collections::HashSet;

use slim_core::provider::{
    zen_model, zen_models, OpenCodeApi, OpenCodeZenAdapter, ProviderAdapter, ProviderKind,
    OPENCODE_ZEN_BASE_URL, OPENCODE_ZEN_PUBLIC_KEY,
};

#[test]
fn free_registry_contains_unique_models_only() {
    let models = zen_models();
    let unique = models.iter().map(|model| model.id).collect::<HashSet<_>>();

    assert_eq!(models.len(), unique.len());
    assert_eq!(models.len(), 8);
    for model in models {
        assert!(
            model.id == "big-pickle" || model.id.ends_with("-free"),
            "{id} is not a documented free-tier id",
            id = model.id
        );
        assert!(model.context_window.is_some_and(|tokens| tokens > 0));
        assert!(model.max_output_tokens.is_some_and(|tokens| tokens > 0));
        assert_ne!(model.api, OpenCodeApi::AnthropicMessages);
    }
}

#[test]
fn paid_zen_models_are_not_in_the_free_table() {
    for id in ["claude-sonnet-4-6", "gpt-5.6-luna", "kimi-k2.5"] {
        assert!(zen_model(id).is_none(), "{id} must stay paid-only");
        assert!(OpenCodeZenAdapter::new(OPENCODE_ZEN_BASE_URL, id, "key", None).is_err());
    }
}

#[test]
fn public_bearer_and_session_header_reach_every_request() {
    use slim_core::provider::ProviderMessage;
    let messages = [ProviderMessage::user("inspect the workspace")];
    let adapter = OpenCodeZenAdapter::new(
        OPENCODE_ZEN_BASE_URL,
        "big-pickle",
        OPENCODE_ZEN_PUBLIC_KEY,
        None,
    )
    .expect("adapter")
    .with_session_id("conversation-one");
    let prepared = adapter
        .prepare_messages_request_with_tools_checked(&messages, &[])
        .unwrap();
    let compact = adapter
        .prepare_compaction_request_checked(&messages)
        .unwrap();

    for headers in [
        adapter.build_request("inspect").headers,
        adapter.build_messages_request(&messages).headers,
        prepared.headers().to_vec(),
        compact.headers().to_vec(),
    ] {
        let session: Vec<_> = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-opencode-session"))
            .collect();
        assert_eq!(session.len(), 1, "session header must be sent exactly once");
        assert!(!session[0].1.is_empty());
        assert!(headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer public"));
        assert!(headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("user-agent")
                && value.starts_with("slim/")));
    }
}

#[test]
fn chat_model_uses_chat_completions() {
    let adapter = OpenCodeZenAdapter::new(
        OPENCODE_ZEN_BASE_URL,
        "nemotron-3-ultra-free",
        "zen-key",
        None,
    )
    .expect("adapter")
    .with_system_prompt("skill-marker");

    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");

    assert!(request.url.ends_with("/v1/chat/completions"));
    assert!(request
        .headers
        .iter()
        .any(|(name, value)| name == "Authorization" && value == "Bearer zen-key"));
    assert_eq!(body["messages"][0]["content"], "skill-marker");
}

#[test]
fn responses_free_model_uses_responses_wire() {
    let adapter = OpenCodeZenAdapter::new(
        OPENCODE_ZEN_BASE_URL,
        "muse-spark-1.3-contributor-free",
        "zen-key",
        Some("high"),
    )
    .expect("adapter")
    .with_max_output_tokens(321);

    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");

    assert!(request.url.ends_with("/v1/responses"));
    assert_eq!(body["max_output_tokens"], 321);
    assert_eq!(body["reasoning"]["effort"], "high");
    assert!(!request.headers.iter().any(|(name, _)| matches!(
        name.as_str(),
        "chatgpt-account-id" | "OpenAI-Beta" | "originator"
    )));
    assert!(OpenCodeZenAdapter::new(
        OPENCODE_ZEN_BASE_URL,
        "muse-spark-1.3-contributor-free",
        "zen-key",
        Some("max"),
    )
    .is_err());
}

#[test]
fn models_without_reasoning_levels_drop_configured_effort() {
    // A global effort (slim.toml) must not break models with no effort knob:
    // the field is dropped from the wire body instead of erroring.
    let adapter = OpenCodeZenAdapter::new(OPENCODE_ZEN_BASE_URL, "big-pickle", "key", Some("high"))
        .expect("effort dropped, not rejected");

    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");
    assert!(body.get("reasoning_effort").is_none());

    assert!(OpenCodeZenAdapter::new(
        OPENCODE_ZEN_BASE_URL,
        "deepseek-v4-flash-free",
        "key",
        Some("low"),
    )
    .is_ok());
    assert!(OpenCodeZenAdapter::new(
        OPENCODE_ZEN_BASE_URL,
        "deepseek-v4-flash-free",
        "key",
        Some("medium"),
    )
    .is_err());
}

#[test]
fn image_rejection_is_enforced_per_model() {
    use slim_core::provider::{ProviderContentBlock, ProviderMessage};
    let history = [ProviderMessage::user("old image")
        .with_content_blocks(vec![ProviderContentBlock::image("image/png", "aGk=")])];

    let text_only =
        OpenCodeZenAdapter::new(OPENCODE_ZEN_BASE_URL, "nemotron-3-ultra-free", "key", None)
            .unwrap();
    assert!(text_only
        .prepare_messages_request_checked(&history)
        .is_err());

    let vision =
        OpenCodeZenAdapter::new(OPENCODE_ZEN_BASE_URL, "mimo-v2.5-free", "key", None).unwrap();
    assert!(vision.prepare_messages_request_checked(&history).is_ok());
}

#[test]
fn logical_provider_is_distinct_from_wire_protocol() {
    let adapter =
        OpenCodeZenAdapter::new(OPENCODE_ZEN_BASE_URL, "big-pickle", "key", None).expect("adapter");
    assert_eq!(adapter.kind(), ProviderKind::OpenCodeZen);
    assert_eq!(adapter.wire_kind(), ProviderKind::OpenAiCompatible);
}
