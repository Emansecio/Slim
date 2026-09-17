//! Adversarial coverage for provider stream-event parsing (RODADA 2 —
//! Sifter). `ProviderAdapter::parse_event` is the public boundary where raw
//! provider JSON becomes `ProviderEvent`s; these tests feed malformed,
//! boundary, and contradictory event shapes to the OpenAI-compatible and
//! Anthropic adapters.

use serde_json::{json, Value};
use slim_core::provider::{
    AnthropicAdapter, OpenAiCompatibleAdapter, ProviderAdapter, ProviderConfig, ProviderError,
    ProviderEvent,
};

fn openai() -> OpenAiCompatibleAdapter {
    OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://unit.test/v1/chat/completions",
        "test-model",
        "SECRET-KEY-AAA",
    ))
    .expect("adapter")
}

fn anthropic() -> AnthropicAdapter {
    AnthropicAdapter::new(ProviderConfig::anthropic(
        "https://unit.test/v1/messages",
        "test-model",
        "SECRET-KEY-BBB",
    ))
    .expect("adapter")
}

fn parse(
    adapter: &impl ProviderAdapter,
    value: Value,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    adapter.parse_event(&value)
}

// ---------------------------------------------------------------------------
// OpenAI-compatible adapter
// ---------------------------------------------------------------------------

/// Control for the JSON-RPC `error:null` bug found in the LSP/MCP dispatchers:
/// the provider layer already guards with `!error.is_null()`, so a response
/// carrying an explicit null error member parses normally.
#[test]
fn openai_null_error_member_is_not_an_error() {
    let events = parse(
        &openai(),
        json!({"error": null, "choices": [{"delta": {"content": "hi"}}]}),
    )
    .expect("null error is fine");
    assert_eq!(events, vec![ProviderEvent::TextDelta("hi".into())]);
}

#[test]
fn openai_error_payloads_fail_and_redact_the_api_key() {
    for error in [
        json!({"error": {"message": "bad key SECRET-KEY-AAA supplied"}}),
        json!({"error": "string SECRET-KEY-AAA error"}),
        json!({"error": 42}),
        json!({"error": true}),
    ] {
        let result = parse(&openai(), error);
        let failure = result.expect_err("error payloads must fail");
        // ProviderError has no Display impl; Debug shows variant payloads.
        let text = format!("{failure:?}");
        assert!(
            !text.contains("SECRET-KEY-AAA"),
            "api key leaked into error: {text}"
        );
    }
}

#[test]
fn openai_non_object_and_missing_choices_parse_to_empty() {
    for value in [
        json!(null),
        json!(5),
        json!("text"),
        json!([1, 2]),
        json!({}),
        json!({"choices": "bogus"}),
        json!({"choices": null}),
        json!({"choices": {}}),
    ] {
        let events = parse(&openai(), value).expect("lenient parse");
        assert!(events.is_empty(), "{events:?}");
    }
}

#[test]
fn openai_delta_content_non_string_or_empty_is_dropped() {
    for content in [json!(42), json!({"text": "x"}), json!([]), json!("")] {
        let events = parse(
            &openai(),
            json!({"choices": [{"delta": {"content": content}}]}),
        )
        .expect("parse");
        assert!(events.is_empty(), "{events:?}");
    }
}

#[test]
fn openai_usage_cache_exceeding_total_input_fails() {
    let result = parse(
        &openai(),
        json!({"usage": {"prompt_tokens": 5, "prompt_tokens_details": {"cached_tokens": 10}}}),
    );
    let error = result.expect_err("cached > total");
    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
}

#[test]
fn openai_usage_cache_addition_overflow_fails() {
    let result = parse(
        &openai(),
        json!({"usage": {
            "prompt_tokens": 1,
            "prompt_tokens_details": {
                "cached_tokens": u64::MAX,
                "cache_write_tokens": 1
            }
        }}),
    );
    let error = result.expect_err("cache sum overflow");
    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
}

#[test]
fn openai_usage_above_u64_is_silently_absent() {
    // 2^64 does not fit u64: as_u64 yields None and the field is treated as
    // missing — no Usage events and no error. `json!` cannot construct the
    // out-of-range literal, so the document arrives as raw text.
    let value: Value = serde_json::from_str(
        r#"{"usage": {"prompt_tokens": 18446744073709551616, "completion_tokens": 18446744073709551616}}"#,
    )
    .expect("json parses as f64");
    let events = parse(&openai(), value).expect("parse");
    assert!(events.is_empty());
}

#[test]
fn openai_tool_call_non_object_entry_becomes_malformed_delta() {
    let events = parse(
        &openai(),
        json!({"choices": [{"delta": {"tool_calls": [42, "x", null, {"function": {"name": "f", "arguments": "{}"}}]}}]}),
    )
    .expect("parse");
    // 42 and "x" are non-object → malformed deltas; null is skipped; the
    // well-formed entry has no index/id → a ToolCallDelta plus a bonus
    // ToolCall (name + parseable arguments, provider.rs:4594-4599).
    assert!(
        events
            .iter()
            .filter(|event| matches!(event, ProviderEvent::ToolCallDelta { .. }))
            .count()
            >= 3
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, ProviderEvent::ToolCall { .. })));
}

#[test]
fn openai_tool_call_index_over_u32_becomes_malformed() {
    let events = parse(
        &openai(),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 4294967296u64, "id": "c1"}]}}]}),
    )
    .expect("parse");
    assert_eq!(
        events,
        vec![ProviderEvent::ToolCallDelta {
            index: None,
            id: None,
            name: None,
            arguments: String::new(),
        }]
    );
}

#[test]
fn openai_tool_call_arguments_non_string_is_stringified() {
    let events = parse(
        &openai(),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "f", "arguments": {"nested": 1}}}]}}]}),
    )
    .expect("parse");
    assert_eq!(
        events[0],
        ProviderEvent::ToolCallDelta {
            index: Some(0),
            id: None,
            name: Some("f".into()),
            arguments: "{\"nested\":1}".into(),
        }
    );
}

/// Spec gap: a `finish_reason` that is not a string (or is null) is silently
/// dropped — no Stopped event and no error. A stream that ends after such a
/// chunk leaves the runtime without a terminal signal.
#[test]
fn openai_finish_reason_non_string_silently_dropped() {
    for reason in [json!(null), json!(42), json!({"r": "stop"}), json!(true)] {
        let events = parse(
            &openai(),
            json!({"choices": [{"delta": {}, "finish_reason": reason}]}),
        )
        .expect("parse");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::Stopped { .. })),
            "{events:?}"
        );
    }
    let events = parse(
        &openai(),
        json!({"choices": [{"delta": {}, "finish_reason": "length"}]}),
    )
    .expect("parse");
    assert_eq!(
        events,
        vec![ProviderEvent::Stopped {
            reason: "length".into()
        }]
    );
}

// ---------------------------------------------------------------------------
// Anthropic adapter
// ---------------------------------------------------------------------------

#[test]
fn anthropic_error_events_fail_and_redact_the_api_key() {
    let result = parse(
        &anthropic(),
        json!({"type": "error", "error": {"message": "invalid SECRET-KEY-BBB token"}}),
    );
    let failure = result.expect_err("error event");
    let text = format!("{failure:?}");
    assert!(!text.contains("SECRET-KEY-BBB"), "api key leaked: {text}");
}

#[test]
fn anthropic_unknown_missing_and_non_string_types_are_ignored() {
    for value in [
        json!({"type": "ping"}),
        json!({"type": 42}),
        json!({}),
        json!("text"),
        json!(7),
    ] {
        let events = parse(&anthropic(), value).expect("lenient");
        assert!(events.is_empty(), "{events:?}");
    }
}

/// Spec gap: `content_block_delta` requires `index` for EVERY delta kind —
/// including plain text. A missing/oversized index yields
/// `MalformedToolCall` even though no tool call is involved, so the error
/// variant mislabels the failure.
#[test]
fn anthropic_text_delta_without_index_fails_as_malformed_tool_call() {
    let result = parse(
        &anthropic(),
        json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "hi"}}),
    );
    assert!(matches!(result, Err(ProviderError::MalformedToolCall)));

    let result = parse(
        &anthropic(),
        json!({"type": "content_block_delta", "index": 4294967296u64, "delta": {"type": "text_delta", "text": "hi"}}),
    );
    assert!(matches!(result, Err(ProviderError::MalformedToolCall)));

    let events = parse(
        &anthropic(),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
    )
    .expect("valid text delta");
    assert_eq!(events, vec![ProviderEvent::TextDelta("hi".into())]);
}

#[test]
fn anthropic_content_block_stop_requires_index() {
    assert!(matches!(
        parse(&anthropic(), json!({"type": "content_block_stop"})),
        Err(ProviderError::MalformedToolCall)
    ));
}

/// Spec gap: `tool_use` blocks with missing id/name are accepted with empty
/// identities, and a non-object `input` is stringified into arguments — a
/// scalar `5` becomes the JSON text `5`.
#[test]
fn anthropic_tool_use_missing_identity_and_scalar_input() {
    let events = parse(
        &anthropic(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "input": 5}}),
    )
    .expect("parse");
    assert_eq!(
        events[0],
        ProviderEvent::ToolCallStart {
            index: 0,
            id: String::new(),
            name: String::new(),
        }
    );
    assert_eq!(
        events[1],
        ProviderEvent::ToolCall {
            name: String::new(),
            arguments: "5".into(),
        }
    );
}

#[test]
fn anthropic_tool_use_non_tool_blocks_do_not_emit() {
    let events = parse(
        &anthropic(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "hi"}}),
    )
    .expect("parse");
    assert!(events.is_empty());
}

#[test]
fn anthropic_message_start_usage_overflow_fails() {
    let result = parse(
        &anthropic(),
        json!({"type": "message_start", "message": {"usage": {
            "input_tokens": u64::MAX,
            "cache_read_input_tokens": 1
        }}}),
    );
    let error = result.expect_err("usage overflow");
    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
}

#[test]
fn anthropic_message_delta_emits_usage_and_stop() {
    let events = parse(
        &anthropic(),
        json!({"type": "message_delta", "usage": {"output_tokens": 7}, "delta": {"stop_reason": "end_turn"}}),
    )
    .expect("parse");
    assert!(events
        .iter()
        .any(|event| matches!(event, ProviderEvent::Stopped { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, ProviderEvent::Usage { .. })));
}

#[test]
fn adapter_constructors_reject_kind_mismatch() {
    let result = OpenAiCompatibleAdapter::new(ProviderConfig::anthropic("e", "m", "k"));
    assert!(result.is_err());
    let result = AnthropicAdapter::new(ProviderConfig::openai("e", "m", "k"));
    assert!(result.is_err());
}

#[test]
fn anthropic_adapter_rejects_unknown_reasoning_effort() {
    let config = ProviderConfig::anthropic("e", "m", "k").with_reasoning_effort("ultra");
    assert!(AnthropicAdapter::new(config).is_err());
    let config = ProviderConfig::anthropic("e", "m", "k").with_reasoning_effort("high");
    assert!(AnthropicAdapter::new(config).is_ok());
}
