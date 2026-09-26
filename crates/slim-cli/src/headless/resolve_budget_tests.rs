#[test]
fn unknown_context_window_requires_explicit_metadata() {
    assert!(super::require_model_context_window(None).is_err());
    assert!(super::require_model_context_window(Some(0)).is_err());
    assert_eq!(
        super::require_model_context_window(Some(128_000)).unwrap(),
        128_000
    );
    assert_eq!(
        super::known_model_context_window(
            slim_core::provider::ProviderKind::OpenAiCodex,
            "unknown-model",
            None
        ),
        None
    );
    assert_eq!(
        super::resolve_context_window_tokens(Some(65_536)).unwrap(),
        65_536
    );
}
use super::{
    command_code_context_window, command_code_zero_data_retention, execute_provider_turn,
    resolve_max_mutating_tool_calls, resolve_max_output_tokens, resolve_max_read_tool_calls,
    resolve_max_result_bytes, resolve_max_turns, resolve_timeout_secs, ProviderRequest,
    ProviderRunOptions, DEFAULT_PROVIDER_TIMEOUT_SECS,
};
use slim_core::runtime::AgentLoopConfig;
use std::sync::{Mutex, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
    let _guard = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("lock");
    let saved = vars
        .iter()
        .map(|(name, _)| (*name, std::env::var_os(name)))
        .collect::<Vec<_>>();
    for (name, value) in vars {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }
    f();
    for (name, prior) in saved {
        match prior {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }
}

#[test]
fn explicit_options_override_env_defaults() {
    with_env(
        &[
            ("SLIM_MAX_MUTATING_TOOL_CALLS", Some("64")),
            ("SLIM_MAX_READ_TOOL_CALLS", Some("128")),
            ("SLIM_MAX_TURNS", Some("32")),
        ],
        || {
            let options = ProviderRunOptions::default()
                .with_max_tool_calls(8)
                .with_max_read_tool_calls(24)
                .with_max_turns(4);
            assert_eq!(
                resolve_max_mutating_tool_calls(&options).expect("mutating"),
                8
            );
            assert_eq!(resolve_max_read_tool_calls(&options).expect("read"), 24);
            assert_eq!(resolve_max_turns(&options).expect("turns"), 4);
        },
    );
}

#[test]
fn env_vars_apply_when_options_unset() {
    with_env(
        &[
            ("SLIM_MAX_MUTATING_TOOL_CALLS", Some("48")),
            ("SLIM_MAX_READ_TOOL_CALLS", Some("120")),
            ("SLIM_MAX_TURNS", Some("64")),
        ],
        || {
            let options = ProviderRunOptions::default();
            assert_eq!(
                resolve_max_mutating_tool_calls(&options).expect("mutating"),
                48
            );
            assert_eq!(resolve_max_read_tool_calls(&options).expect("read"), 120);
            assert_eq!(resolve_max_turns(&options).expect("turns"), 64);
        },
    );
}

#[test]
fn defaults_apply_without_env_or_options() {
    with_env(
        &[
            ("SLIM_MAX_MUTATING_TOOL_CALLS", None),
            ("SLIM_MAX_READ_TOOL_CALLS", None),
            ("SLIM_MAX_TURNS", None),
        ],
        || {
            let options = ProviderRunOptions::default();
            assert_eq!(
                resolve_max_mutating_tool_calls(&options).expect("mutating"),
                AgentLoopConfig::DEFAULT_MAX_MUTATING_TOOL_CALLS
            );
            assert_eq!(
                resolve_max_read_tool_calls(&options).expect("read"),
                AgentLoopConfig::DEFAULT_MAX_READ_TOOL_CALLS
            );
            assert_eq!(
                resolve_max_turns(&options).expect("turns"),
                AgentLoopConfig::DEFAULT_MAX_TURNS
            );
        },
    );
}

#[test]
fn max_turns_hard_cap_clamps_options_and_env() {
    with_env(&[("SLIM_MAX_TURNS", Some("4096"))], || {
        assert_eq!(
            resolve_max_turns(&ProviderRunOptions::default()).expect("env clamp"),
            1024
        );
        let options = ProviderRunOptions::default().with_max_turns(2048);
        assert_eq!(resolve_max_turns(&options).expect("options clamp"), 1024);
    });
}

#[test]
fn max_turns_env_zero_is_rejected() {
    with_env(&[("SLIM_MAX_TURNS", Some("0"))], || {
        let error = resolve_max_turns(&ProviderRunOptions::default()).expect_err("zero");
        assert!(matches!(
            error,
            slim_core::ProviderError::InvalidResponse { .. }
        ));
    });
}

#[test]
fn explicit_zero_max_turns_is_preserved() {
    with_env(&[("SLIM_MAX_TURNS", Some("64"))], || {
        let options = ProviderRunOptions::default().with_max_turns(0);
        assert_eq!(resolve_max_turns(&options).expect("zero option"), 0);
    });
}

#[test]
fn default_max_output_uses_catalog_when_the_window_is_known() {
    with_env(&[("SLIM_MAX_OUTPUT_TOKENS", None)], || {
        let tokens = resolve_max_output_tokens(
            None,
            slim_core::provider::ProviderKind::OpenAiCodex,
            "gpt-5.6-sol",
        )
        .expect("default output cap");
        assert_eq!(tokens, 128_000);
        assert_eq!(
            resolve_max_output_tokens(None, slim_core::provider::ProviderKind::Xai, "grok-4.3",)
                .expect("xai"),
            30_000
        );
    });
}

#[test]
fn explicit_and_env_max_output_are_bounded_by_catalog() {
    with_env(&[("SLIM_MAX_OUTPUT_TOKENS", Some("128001"))], || {
        assert!(resolve_max_output_tokens(
            None,
            slim_core::provider::ProviderKind::OpenAiCodex,
            "gpt-5.6-sol",
        )
        .is_err());
    });
    assert!(resolve_max_output_tokens(
        Some(128_001),
        slim_core::provider::ProviderKind::OpenAiCodex,
        "gpt-5.6-sol",
    )
    .is_err());
}

#[test]
fn max_output_options_and_env_beat_catalog() {
    with_env(&[("SLIM_MAX_OUTPUT_TOKENS", Some("2048"))], || {
        assert_eq!(
            resolve_max_output_tokens(
                None,
                slim_core::provider::ProviderKind::OpenAiCodex,
                "gpt-5.6-sol",
            )
            .expect("env"),
            2048
        );
        assert_eq!(
            resolve_max_output_tokens(
                Some(512),
                slim_core::provider::ProviderKind::OpenAiCodex,
                "gpt-5.6-sol",
            )
            .expect("options"),
            512
        );
    });
}

#[test]
fn compatible_and_command_code_fall_back_to_default_max_output() {
    with_env(&[("SLIM_MAX_OUTPUT_TOKENS", None)], || {
        assert_eq!(
            resolve_max_output_tokens(
                None,
                slim_core::provider::ProviderKind::OpenAiCompatible,
                "gpt-4o-mini",
            )
            .expect("compat"),
            slim_core::provider::DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert_eq!(
            resolve_max_output_tokens(
                None,
                slim_core::provider::ProviderKind::CommandCode,
                slim_core::provider::COMMANDCODE_DEFAULT_MODEL,
            )
            .expect("command-code"),
            slim_core::provider::DEFAULT_MAX_OUTPUT_TOKENS
        );
    });
}

#[test]
fn timeout_zero_is_rejected_and_idle_clamps_to_one_hour() {
    with_env(&[("SLIM_TIMEOUT_SECS", None)], || {
        let error = resolve_timeout_secs(Some(0)).expect_err("zero");
        assert!(matches!(
            error,
            slim_core::ProviderError::InvalidResponse { .. }
        ));
        assert_eq!(
            resolve_timeout_secs(None).expect("default").as_secs(),
            DEFAULT_PROVIDER_TIMEOUT_SECS
        );
        assert_eq!(
            resolve_timeout_secs(Some(9_000)).expect("clamp").as_secs(),
            3600
        );
    });
    with_env(&[("SLIM_TIMEOUT_SECS", Some("0"))], || {
        assert!(resolve_timeout_secs(None).is_err());
    });
}

#[test]
fn command_code_context_window_uses_cached_live_only_models() {
    let cache = std::env::temp_dir().join(format!(
        "slim-cmd-models-{}-cached.json",
        std::process::id()
    ));
    std::fs::write(
        &cache,
        br#"{"version":1,"models":[
            {"id":"live-only/model","name":"Live Only","context_window":777000},
            {"id":"claude-sonnet-5","name":"Claude Sonnet 5","context_window":111111}
        ]}"#,
    )
    .expect("cache");
    let cache_str = cache.to_string_lossy().into_owned();
    with_env(
        &[("SLIM_COMMANDCODE_MODELS_FILE", Some(&cache_str))],
        || {
            // A model absent from the static registry resolves through the
            // cached live snapshot.
            assert_eq!(
                command_code_context_window("live-only/model"),
                Some(777_000)
            );
            // The cache wins over the static registry for shared ids.
            assert_eq!(
                command_code_context_window("claude-sonnet-5"),
                Some(111_111)
            );
        },
    );
    let _ = std::fs::remove_file(&cache);
}

#[test]
fn command_code_context_window_falls_back_to_static_registry() {
    let missing = std::env::temp_dir().join(format!(
        "slim-cmd-models-{}-missing.json",
        std::process::id()
    ));
    let missing_str = missing.to_string_lossy().into_owned();
    with_env(
        &[("SLIM_COMMANDCODE_MODELS_FILE", Some(&missing_str))],
        || {
            assert_eq!(
                command_code_context_window("deepseek/deepseek-v4.1-flash"),
                Some(1_000_000)
            );
            assert_eq!(command_code_context_window("totally/unknown"), None);
        },
    );
}

#[test]
fn command_code_zdr_requires_explicit_affirmative() {
    with_env(&[("SLIM_CMD_ZDR", None), ("CMD_ZDR", None)], || {
        assert!(!command_code_zero_data_retention());
    });
    for value in ["1", "true", "TRUE", " yes ", "on"] {
        with_env(&[("SLIM_CMD_ZDR", Some(value)), ("CMD_ZDR", None)], || {
            assert!(command_code_zero_data_retention(), "value {value}")
        });
    }
    for value in ["0", "false", "no", "yess", ""] {
        with_env(&[("SLIM_CMD_ZDR", Some(value)), ("CMD_ZDR", None)], || {
            assert!(!command_code_zero_data_retention(), "value {value}")
        });
    }
    with_env(&[("SLIM_CMD_ZDR", None), ("CMD_ZDR", Some("1"))], || {
        assert!(command_code_zero_data_retention());
    });
}

#[test]
fn max_result_bytes_zero_is_rejected_and_hard_cap_is_one_mib() {
    with_env(&[("SLIM_MAX_RESULT_BYTES", None)], || {
        let error = resolve_max_result_bytes(Some(0)).expect_err("zero");
        assert!(matches!(
            error,
            slim_core::ProviderError::InvalidResponse { .. }
        ));
        assert_eq!(resolve_max_result_bytes(None).expect("default"), 16 * 1024);
        assert_eq!(
            resolve_max_result_bytes(Some(2 * 1024 * 1024)).expect("clamp"),
            1024 * 1024
        );
    });
}

#[test]
fn opencode_rejects_max_output_above_model_metadata() {
    with_env(
        &[
            ("SLIM_MAX_OUTPUT_TOKENS", None),
            ("SLIM_CONTEXT_WINDOW_TOKENS", None),
        ],
        || {
            let request = ProviderRequest {
                prompt: "hi".into(),
                mode: slim_core::OperatingMode::Auto,
                kind: slim_core::provider::ProviderKind::OpenCodeGo,
                endpoint: slim_core::provider::OPENCODE_GO_BASE_URL.into(),
                model: slim_core::provider::OPENCODE_GO_DEFAULT_MODEL.into(),
                api_key: "key".into(),
                account_id: None,
                timeout: std::time::Duration::from_secs(1),
            };
            match execute_provider_turn(
                request,
                None,
                ProviderRunOptions::default().with_max_output_tokens(384_001),
            ) {
                Err(error) => assert!(
                    matches!(
                        error,
                        slim_core::ProviderError::InvalidResponse { ref message }
                            if message.contains("OpenCode Go context or output limit")
                    ),
                    "{error:?}"
                ),
                Ok(_) => panic!("OpenCode overflow must reject before the network"),
            }
        },
    );
}
