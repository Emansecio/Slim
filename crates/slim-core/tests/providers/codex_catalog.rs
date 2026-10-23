use slim_core::provider::{
    codex_model, codex_models, codex_models_url, parse_codex_catalog, resolve_codex_context_window,
    CODEX_BUNDLED_CONTEXT_WINDOW, CODEX_CATALOG_CLIENT_VERSION, CODEX_MAX_OUTPUT_TOKENS,
};

#[test]
fn bundled_codex_models_follow_the_account_catalog_and_its_windows() {
    for id in ["gpt-5.6-sol", "sol", "gpt-5.6-terra", "terra", "gpt-5.6-luna", "luna"] {
        let model = codex_model(id).unwrap_or_else(|| panic!("missing Codex metadata for {id}"));
        assert_eq!(model.context_window, CODEX_BUNDLED_CONTEXT_WINDOW);
        assert!(model.context_window > 32_000);
        assert_eq!(model.max_output_tokens, CODEX_MAX_OUTPUT_TOKENS);
        assert!(model.reasoning_levels.contains(&"high"));
    }
    // Retired subscription models must not survive in the fallback list.
    assert!(codex_model("astra").is_none());
    assert!(codex_model("gpt-6-astra").is_none());
    assert!(codex_model("gpt-6.1-sol").is_none());
    assert_eq!(codex_models().len(), 3);
    assert!(codex_model("deepseek-v4-flash").is_none());
}

#[test]
fn catalog_rows_carry_display_name_levels_and_visibility() {
    let live = parse_codex_catalog(
        br#"{
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6-Sol",
                    "context_window": 272000,
                    "visibility": "list",
                    "supported_reasoning_levels": [
                        {"effort": "low"},
                        {"effort": "ultra"}
                    ]
                },
                {"slug": "gpt-reserve", "display_name": "GPT-Reserve", "visibility": "hide"},
                {"slug": "codex-auto-review"}
            ]
        }"#,
    )
    .expect("catalog");

    assert_eq!(live[0].display_name.as_deref(), Some("GPT-5.6-Sol"));
    assert_eq!(live[0].reasoning_levels, vec!["low", "ultra"]);
    assert!(live[0].visible);
    assert!(!live[1].visible);
    // A row without a declared window keeps the bundled figure, so the picker
    // still lists it instead of dropping the model.
    assert_eq!(live[2].context_window, CODEX_BUNDLED_CONTEXT_WINDOW);
    assert!(live[2].visible);
    assert!(live[2].reasoning_levels.is_empty());
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
