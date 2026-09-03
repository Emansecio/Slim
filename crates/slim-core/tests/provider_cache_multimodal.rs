use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use slim_core::provider::{
    AnthropicAdapter, HttpProviderClient, OpenAiCodexAdapter, OpenAiCompatibleAdapter,
    ProviderAdapter, ProviderCache, ProviderCacheStats, ProviderConfig, ProviderContentBlock,
    ProviderError, ProviderEvent, ProviderMessage, UsageBreakdown,
};

#[test]
fn cache_key_hides_credentials_and_includes_tenant_and_wire_config() {
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
    let first_prompt_route = adapter
        .prepare_messages_request_with_tools_checked(&messages, &tools_a)
        .expect("first prepared request")
        .prompt_cache_routing_key()
        .to_string();
    let other_prompt_route = same_origin_different_query
        .prepare_messages_request_with_tools_checked(&messages, &tools_a)
        .expect("other prepared request")
        .prompt_cache_routing_key()
        .to_string();
    assert_ne!(first_prompt_route, other_prompt_route);
    assert!(!first_prompt_route.contains("fixture-secret"));
    assert!(!other_prompt_route.contains("other-secret"));
    assert_ne!(
        first,
        same_origin_different_query.cache_key_with_tools(&messages, &tools_a),
        "credential scope must partition shared cache without exposing the secret"
    );

    let query_secret_only = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions?api_key=other-secret",
        "model-a",
        "fixture-secret",
    ))
    .expect("adapter");
    assert_ne!(
        first,
        query_secret_only.cache_key_with_tools(&messages, &tools_a),
        "secret query values must partition cache even when the auth header is unchanged"
    );

    let semantic_query = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions?api_key=fixture-secret&deployment=blue",
        "model-a",
        "fixture-secret",
    ))
    .expect("adapter");
    assert_ne!(
        first,
        semantic_query.cache_key_with_tools(&messages, &tools_a),
        "non-secret query configuration must partition cache identity"
    );

    let codex_a = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://example.invalid/v1",
        "model-a",
        "token-a",
        "account-a",
    ))
    .expect("codex a");
    let codex_b = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "https://example.invalid/v1",
        "model-a",
        "token-b",
        "account-b",
    ))
    .expect("codex b");
    assert_ne!(
        codex_a.cache_key_with_tools(&messages, &tools_a),
        codex_b.cache_key_with_tools(&messages, &tools_a),
        "non-secret account routing header must partition cache identity"
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

    for semantically_different in [
        ProviderConfig::openai(
            "https://example.invalid/v1/chat/completions?api_key=fixture-secret",
            "model-a",
            "fixture-secret",
        )
        .with_system_prompt("different system contract"),
        ProviderConfig::openai(
            "https://example.invalid/v1/chat/completions?api_key=fixture-secret",
            "model-a",
            "fixture-secret",
        )
        .with_max_output_tokens(99),
        ProviderConfig::openai(
            "https://example.invalid/v1/chat/completions?api_key=fixture-secret",
            "model-a",
            "fixture-secret",
        )
        .with_reasoning_effort("high"),
    ] {
        let adapter = OpenAiCompatibleAdapter::new(semantically_different).expect("adapter");
        assert_ne!(
            first,
            adapter.cache_key_with_tools(&messages, &tools_a),
            "wire-affecting provider configuration must partition cache identity"
        );
    }
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
    let events = vec![
        ProviderEvent::TextDelta("cached".into()),
        ProviderEvent::Stopped {
            reason: "end_turn".into(),
        },
    ];

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
fn cached_replay_is_bounded_and_observes_ready_cancellation() {
    let adapter = OpenAiCompatibleAdapter::new(ProviderConfig::openai(
        "https://example.invalid/v1/chat/completions",
        "model-a",
        "secret-a",
    ))
    .expect("adapter");
    let cache = Arc::new(ProviderCache::new());
    let messages = [ProviderMessage::user("hello")];
    cache.insert_for_adapter(
        &adapter,
        &messages,
        &[],
        vec![
            ProviderEvent::TextDelta("cached".into()),
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
        ],
    );
    let client =
        HttpProviderClient::new_with_cache(adapter, Duration::from_secs(1), cache).expect("client");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut seen = 0;
    let error = runtime
        .block_on(client.stream_messages_with_tools_cancellable(
            &messages,
            &[],
            std::future::ready(()),
            |_| seen += 1,
        ))
        .expect_err("ready cancellation wins before replay");
    assert_eq!(error, ProviderError::Cancelled);
    assert_eq!(seen, 0);

    let mut replayed = Vec::new();
    runtime
        .block_on(client.stream_messages_with_tools_cancellable(
            &messages,
            &[],
            std::future::pending(),
            |event| replayed.push(event),
        ))
        .expect("cached replay");
    assert!(matches!(
        replayed.first(),
        Some(ProviderEvent::ResponseCacheHit)
    ));

    let oversized = ProviderCache::new();
    let mut events = vec![ProviderEvent::TextDelta("x".into()); 4_097];
    events.push(ProviderEvent::Stopped {
        reason: "stop".into(),
    });
    oversized.insert("oversized", events);
    assert!(oversized.is_empty());

    for index in 0..128 {
        oversized.insert(
            format!("entry-{index}"),
            vec![ProviderEvent::Stopped {
                reason: "stop".into(),
            }],
        );
    }
    oversized.insert(
        "entry-over-cap",
        vec![ProviderEvent::Stopped {
            reason: "stop".into(),
        }],
    );
    assert_eq!(oversized.len(), 128, "cache entry count is bounded");

    let payload_bounded = ProviderCache::new();
    payload_bounded.insert(
        "oversized-payload",
        vec![
            ProviderEvent::TextDelta("x".repeat(2 * 1024 * 1024)),
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
        ],
    );
    assert!(
        payload_bounded.is_empty(),
        "single payload bytes are bounded"
    );

    let total_bounded = ProviderCache::new();
    for index in 0..6 {
        total_bounded.insert(
            format!("large-{index}"),
            vec![
                ProviderEvent::TextDelta("x".repeat(1_500_000)),
                ProviderEvent::Stopped {
                    reason: "stop".into(),
                },
            ],
        );
    }
    assert_eq!(total_bounded.len(), 5, "total retained bytes are bounded");

    let compacted = ProviderCache::new();
    let mut spare_capacity = String::with_capacity(4 * 1024 * 1024);
    spare_capacity.push('x');
    compacted.insert(
        "compacted",
        vec![
            ProviderEvent::TextDelta(spare_capacity),
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
        ],
    );
    assert_eq!(compacted.len(), 1, "stored strings discard spare capacity");
}

#[test]
fn cache_rejects_incomplete_or_non_success_responses() {
    let cache = ProviderCache::new();
    cache.insert(
        "incomplete",
        vec![ProviderEvent::TextDelta("partial".into())],
    );
    cache.insert(
        "truncated",
        vec![
            ProviderEvent::TextDelta("partial".into()),
            ProviderEvent::Stopped {
                reason: "length".into(),
            },
        ],
    );
    cache.insert(
        "tool-required",
        vec![ProviderEvent::Stopped {
            reason: "tool_calls".into(),
        }],
    );
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
fn cache_strips_billable_usage_before_replay() {
    let cache = ProviderCache::new();
    cache.insert(
        "usage",
        vec![
            ProviderEvent::TextDelta("cached".into()),
            ProviderEvent::UsagePartial {
                input_tokens: 7,
                output_tokens: 0,
                input_complete: true,
                output_complete: false,
            },
            ProviderEvent::UsagePartial {
                input_tokens: 0,
                output_tokens: 3,
                input_complete: false,
                output_complete: true,
            },
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    uncached_input_tokens: 7,
                    output_tokens: 3,
                    ..UsageBreakdown::default()
                },
            },
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
            ProviderEvent::UsageBreakdown {
                usage: UsageBreakdown {
                    uncached_input_tokens: 7,
                    output_tokens: 3,
                    ..UsageBreakdown::default()
                },
            },
        ],
    );
    assert_eq!(
        cache.get("usage"),
        Some(vec![
            ProviderEvent::TextDelta("cached".into()),
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
        ])
    );
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
fn cache_deduplicates_identical_terminal_accounting_but_rejects_conflicts() {
    let cache = ProviderCache::new();
    cache.insert(
        "duplicate-terminal",
        vec![
            ProviderEvent::TextDelta("unsafe".into()),
            ProviderEvent::Usage {
                input_tokens: 3,
                output_tokens: 2,
            },
            ProviderEvent::Usage {
                input_tokens: 3,
                output_tokens: 2,
            },
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
        ],
    );
    assert_eq!(
        cache.get("duplicate-terminal"),
        Some(vec![
            ProviderEvent::TextDelta("unsafe".into()),
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
        ])
    );

    cache.insert(
        "conflicting-terminal",
        vec![
            ProviderEvent::Usage {
                input_tokens: 3,
                output_tokens: 2,
            },
            ProviderEvent::Usage {
                input_tokens: 4,
                output_tokens: 2,
            },
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
        ],
    );
    assert!(cache.get("conflicting-terminal").is_none());

    cache.insert(
        "duplicate-partial",
        vec![
            ProviderEvent::UsagePartial {
                input_tokens: 3,
                output_tokens: 0,
                input_complete: false,
                output_complete: false,
            },
            ProviderEvent::Stopped {
                reason: "stop".into(),
            },
            ProviderEvent::UsagePartial {
                input_tokens: 4,
                output_tokens: 0,
                input_complete: false,
                output_complete: false,
            },
        ],
    );
    assert!(cache.get("duplicate-partial").is_none());
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

#[test]
fn stats_count_hits_misses_and_evictions_and_lru_keeps_hot_keys() {
    let cache = ProviderCache::new();
    let stopped = || {
        vec![ProviderEvent::Stopped {
            reason: "stop".into(),
        }]
    };
    assert_eq!(cache.stats(), ProviderCacheStats::default());

    assert!(cache.get("missing").is_none());
    assert_eq!(cache.stats().misses, 1, "miss counted");
    assert_eq!(cache.stats().hits, 0);

    for index in 0..MAX_CAP {
        cache.insert(format!("entry-{index}"), stopped());
        // Touch each entry once so the insertion order stays cold order.
    }
    assert_eq!(cache.len(), MAX_CAP);
    // Read entry-0 (coldest) and entry-5; entry-0 becomes hot.
    assert!(cache.get("entry-0").is_some());
    assert!(cache.get("entry-5").is_some());
    assert!(cache.get("entry-3").is_some());
    assert_eq!(cache.get("entry-3").unwrap().len(), 1, "hit returns events");

    let before = cache.stats();
    assert!(before.hits >= 3, "reads register hits");
    cache.insert("entry-over-cap", stopped());
    assert_eq!(cache.len(), MAX_CAP, "evicts instead of rejecting");
    assert_eq!(cache.stats().evictions, 1, "eviction counted");
    assert!(
        cache.get("entry-1").is_none(),
        "coldest untouched key evicted"
    );
    assert!(
        cache.get("entry-0").is_some(),
        "recently read key survived under LRU"
    );
    let stats = cache.stats();
    assert!(
        stats.retained_bytes > 0 && stats.entries == MAX_CAP,
        "stats mirror live state"
    );
}

const MAX_CAP: usize = 128;
