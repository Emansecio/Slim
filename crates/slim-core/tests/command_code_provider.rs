use slim_core::provider::{
    command_code_api, command_code_model, parse_command_code_catalog, CommandCodeAdapter,
    CommandCodeApi, ProviderAdapter, ProviderKind, COMMANDCODE_BASE_URL, COMMANDCODE_DEFAULT_MODEL,
};

#[test]
fn default_model_uses_chat_completions() {
    assert_eq!(
        command_code_api(COMMANDCODE_DEFAULT_MODEL),
        CommandCodeApi::ChatCompletions
    );
    assert_eq!(COMMANDCODE_DEFAULT_MODEL, "deepseek/deepseek-v4.1-flash");
}

#[test]
fn claude_models_use_anthropic_messages() {
    assert_eq!(
        command_code_api("claude-sonnet-4-6"),
        CommandCodeApi::AnthropicMessages
    );
    assert_eq!(
        command_code_api("claude-opus-4-7"),
        CommandCodeApi::AnthropicMessages
    );
}

#[test]
fn chat_model_posts_to_provider_chat_completions() {
    let adapter = CommandCodeAdapter::new(
        COMMANDCODE_BASE_URL,
        "deepseek/deepseek-v4.1-flash",
        "cmd-secret",
        None,
    )
    .expect("adapter")
    .with_system_prompt("skill-marker")
    .with_max_output_tokens(1234);

    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");

    assert_eq!(body["max_tokens"], 1234);
    assert_eq!(body["model"], "deepseek/deepseek-v4.1-flash");
    assert_eq!(adapter.model(), "deepseek/deepseek-v4.1-flash");
    assert_eq!(adapter.kind(), ProviderKind::CommandCode);
    assert_eq!(adapter.wire_kind(), ProviderKind::OpenAiCompatible);
    assert!(request.url.ends_with("/provider/v1/chat/completions"));
    assert!(request
        .headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
            && value == "Bearer cmd-secret"));
    assert_eq!(body["messages"][0]["content"], "skill-marker");
}

#[test]
fn claude_model_posts_to_provider_messages() {
    let adapter = CommandCodeAdapter::new(
        COMMANDCODE_BASE_URL,
        "claude-sonnet-4-6",
        "cmd-secret",
        Some("medium"),
    )
    .expect("adapter")
    .with_system_prompt("skill-marker")
    .with_max_output_tokens(1234);

    let request = adapter.build_request("hello");
    let body: serde_json::Value = serde_json::from_str(&request.body).expect("body");

    assert_eq!(body["max_tokens"], 1234);
    assert_eq!(body["output_config"]["effort"], "medium");
    assert_eq!(adapter.wire_kind(), ProviderKind::Anthropic);
    assert!(request.url.ends_with("/provider/v1/messages"));
    assert_eq!(body["system"][0]["text"], "skill-marker");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["cache_control"]["type"], "ephemeral");
}

#[test]
fn live_catalog_ids_not_in_bundle_are_accepted() {
    let adapter =
        CommandCodeAdapter::new(COMMANDCODE_BASE_URL, "stealth/ox-alpha", "cmd-secret", None)
            .expect("live id");

    assert_eq!(adapter.model(), "stealth/ox-alpha");
}

#[test]
fn invalid_model_id_is_rejected() {
    assert!(CommandCodeAdapter::new(COMMANDCODE_BASE_URL, "bad id", "k", None).is_err());
}

#[test]
fn parse_catalog_keeps_live_ids_and_metadata() {
    let body = br#"{
        "object":"list",
        "data":[
            {"id":"claude-sonnet-4-6","name":"Claude Sonnet 4.6","context_length":1000000},
            {"id":"deepseek/deepseek-v4-flash","name":"DeepSeek V4 Flash","context_length":1000000}
        ]
    }"#;

    let models = parse_command_code_catalog(body).expect("catalog");

    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "claude-sonnet-4-6");
    assert_eq!(models[0].name, "Claude Sonnet 4.6");
    assert_eq!(models[0].context_window, 1_000_000);
}

#[test]
fn bundled_default_is_in_fallback_registry() {
    let model = command_code_model(COMMANDCODE_DEFAULT_MODEL).expect("bundled V4.1");
    assert_eq!(model.name, "DeepSeek V4.1 Flash");
    assert_eq!(model.context_window, 1_000_000);
    assert!(command_code_model("deepseek/deepseek-v4-flash").is_some());
}

fn has_zdr_header(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("x-cmd-zdr") && value == "1")
}

#[test]
fn zdr_header_absent_by_default() {
    let adapter = CommandCodeAdapter::new(
        COMMANDCODE_BASE_URL,
        "deepseek/deepseek-v4.1-flash",
        "cmd-secret",
        None,
    )
    .expect("adapter");

    let request = adapter.build_request("hello");
    assert!(!has_zdr_header(&request.headers));
}

#[test]
fn zdr_header_reaches_every_chat_completions_path() {
    use slim_core::ProviderMessage;

    let adapter = CommandCodeAdapter::new(
        COMMANDCODE_BASE_URL,
        "deepseek/deepseek-v4.1-flash",
        "cmd-secret",
        None,
    )
    .expect("adapter")
    .with_zero_data_retention(true);

    let messages = vec![ProviderMessage::user("hello")];
    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {"name": "probe", "parameters": {"type": "object"}}
    })];
    let prepared = adapter
        .prepare_messages_request_with_tools_checked(&messages, &tools)
        .expect("prepared");
    let compaction = adapter
        .prepare_compaction_request_checked(&messages)
        .expect("compaction");

    for headers in [
        adapter.build_request("hello").headers,
        adapter
            .build_messages_request_with_tools(&messages, &tools)
            .headers,
        prepared.headers().to_vec(),
        compaction.headers().to_vec(),
    ] {
        assert!(has_zdr_header(&headers), "missing x-cmd-zdr header");
    }
}

#[test]
fn zdr_header_reaches_every_anthropic_messages_path() {
    use slim_core::ProviderMessage;

    let adapter = CommandCodeAdapter::new(
        COMMANDCODE_BASE_URL,
        "claude-sonnet-4-6",
        "cmd-secret",
        None,
    )
    .expect("adapter")
    .with_zero_data_retention(true);

    let messages = vec![ProviderMessage::user("hello")];
    let prepared = adapter
        .prepare_messages_request_with_tools_checked(&messages, &[])
        .expect("prepared");
    let compaction = adapter
        .prepare_compaction_request_checked(&messages)
        .expect("compaction");

    for headers in [
        adapter.build_request("hello").headers,
        adapter.build_messages_request(&messages).headers,
        prepared.headers().to_vec(),
        compaction.headers().to_vec(),
    ] {
        assert!(has_zdr_header(&headers), "missing x-cmd-zdr header");
    }
}
