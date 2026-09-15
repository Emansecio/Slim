use slim_core::provider::{
    codex_model, codex_models, codex_models_url, parse_codex_catalog, resolve_codex_context_window,
    CODEX_BUNDLED_CONTEXT_WINDOW, CODEX_CATALOG_CLIENT_VERSION,
};

#[test]
fn bundled_gpt_56_aliases_use_codex_catalog_window_not_loop_default() {
    for id in [
        "gpt-5.6-sol",
        "gpt-6-astra",
        "astra",
        "sol",
        "gpt-5.6-terra",
        "terra",
        "gpt-5.6-luna",
        "luna",
    ] {
        let model = codex_model(id).unwrap_or_else(|| panic!("missing Codex metadata for {id}"));
        assert_eq!(model.context_window, CODEX_BUNDLED_CONTEXT_WINDOW);
        assert!(model.context_window > 32_000);
        assert_eq!(model.max_output_tokens, 128_000);
    }
    assert_eq!(codex_models().len(), 4);
    assert!(codex_model("deepseek-v4-flash").is_none());
}

#[test]
fn resolve_prefers_live_catalog_over_bundled_fallback() {
    let live = parse_codex_catalog(
        br#"{
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6 Sol",
                    "context_window": 872000,
                    "max_context_window": 872000,
                    "effective_context_window_percent": 95,
                    "supported_reasoning_levels": ["low", "medium", "high"]
                }
            ]
        }"#,
    )
    .expect("catalog");

    assert_eq!(
        resolve_codex_context_window("gpt-5.6-sol", Some(&live)),
        872_000
    );
    assert_eq!(resolve_codex_context_window("sol", Some(&live)), 872_000);
    assert_eq!(
        resolve_codex_context_window("gpt-5.6-luna", Some(&live)),
        CODEX_BUNDLED_CONTEXT_WINDOW
    );
    assert_eq!(
        resolve_codex_context_window("gpt-5.6-sol", None),
        CODEX_BUNDLED_CONTEXT_WINDOW
    );
}

#[test]
fn live_catalog_uses_context_window_then_max_context_window() {
    let live = parse_codex_catalog(
        br#"{"models":[{"slug":"gpt-5.6-terra","max_context_window":400000}]}"#,
    )
    .expect("max-only catalog");
    assert_eq!(resolve_codex_context_window("terra", Some(&live)), 400_000);
}

#[test]
fn models_url_is_sibling_of_responses_and_pins_client_version() {
    let url = codex_models_url("https://chatgpt.com/backend-api");
    assert!(url.starts_with("https://chatgpt.com/backend-api/codex/models?"));
    assert!(url.contains(&format!("client_version={CODEX_CATALOG_CLIENT_VERSION}")));
    assert!(!url.contains("/codex/responses"));

    let with_query = codex_models_url("https://example.invalid/backend-api?deployment=blue");
    assert!(with_query.contains("/codex/models?"));
    assert!(with_query.contains("deployment=blue"));
    assert!(with_query.contains("client_version="));
}
