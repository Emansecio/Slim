use slim_core::provider::{
    is_xai_model_id, xai_model, xai_models, ProviderAdapter, XaiAdapter, XAI_BASE_URL,
    XAI_DEFAULT_MODEL,
};
use std::collections::HashSet;

#[test]
fn xai_bundle_has_unique_models_and_default_uses_responses_wire() {
    let models = xai_models();
    let unique: HashSet<_> = models.iter().map(|model| model.id).collect();
    assert_eq!((models.len(), unique.len()), (4, 4));
    for model in models {
        assert!(model.context_window > 0);
        assert!(model.max_output_tokens > 0);
        assert!(!model.reasoning_levels.is_empty());
    }
    let default = xai_model(XAI_DEFAULT_MODEL).expect("default model");
    assert_eq!(default.id, "grok-4.5");
    assert!(is_xai_model_id("grok-4.6"));
    assert!(!is_xai_model_id("gpt-5.6-luna"));
    let adapter = XaiAdapter::new(XAI_BASE_URL, XAI_DEFAULT_MODEL, "key", None).expect("adapter");
    assert_eq!(adapter.model(), "grok-4.5");
    let request = adapter.build_messages_request(&[slim_core::ProviderMessage::user("hi")]);
    assert!(request.url.ends_with("/responses"), "{}", request.url);
    assert!(XaiAdapter::new(XAI_BASE_URL, "nope", "key", None).is_err());
}

#[test]
fn xai_validates_effort_and_reads_native_reasoning_events() {
    let adapter = XaiAdapter::new(XAI_BASE_URL, "grok-4.6", "fixture", Some("xhigh")).unwrap();
    let body: serde_json::Value =
        serde_json::from_str(&adapter.build_request("hello").body).unwrap();
    assert_eq!(body["reasoning"]["effort"], "xhigh");
    assert!(XaiAdapter::new(XAI_BASE_URL, "grok-4.5", "fixture", Some("ultra")).is_err());
    assert_eq!(
        adapter
            .parse_event(
                &serde_json::json!({"type":"response.reasoning_text.delta", "delta":"summary"})
            )
            .unwrap(),
        vec![slim_core::provider::ProviderEvent::ReasoningDelta(
            "summary".into()
        )]
    );
}
