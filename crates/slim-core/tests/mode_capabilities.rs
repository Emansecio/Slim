use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use slim_core::runtime::Runtime;
use slim_core::{
    CodeIntelCompleteness, CodeIntelDiagnosticsQuery, CodeIntelMeta, CodeIntelOutcome,
    CodeIntelPositionQuery, CodeIntelServerState, CodeIntelSymbolQuery, CodeIntelligence,
    OperatingMode,
};

struct StubCodeIntel;

fn stub_outcome() -> CodeIntelOutcome {
    CodeIntelOutcome {
        meta: CodeIntelMeta {
            server: "stub".into(),
            state: CodeIntelServerState::Ready,
            completeness: CodeIntelCompleteness::Complete,
            document_version: Some(1),
            stale: false,
            elapsed_ms: 0,
        },
        payload: json!({}),
    }
}

#[async_trait]
impl CodeIntelligence for StubCodeIntel {
    async fn status(&self, _workspace: &Path) -> CodeIntelOutcome {
        stub_outcome()
    }

    async fn definition(&self, _query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        stub_outcome()
    }

    async fn references(&self, _query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        stub_outcome()
    }

    async fn hover(&self, _query: &CodeIntelPositionQuery) -> CodeIntelOutcome {
        stub_outcome()
    }

    async fn symbols(&self, _query: &CodeIntelSymbolQuery) -> CodeIntelOutcome {
        stub_outcome()
    }

    async fn diagnostics(&self, _query: &CodeIntelDiagnosticsQuery) -> CodeIntelOutcome {
        stub_outcome()
    }

    async fn notify_file_changed(&self, _workspace: &Path, _path: &Path, _text: Option<String>) {}
}

fn advertised_names(runtime: &Runtime, mode: OperatingMode) -> Vec<String> {
    runtime
        .advertised_tool_definitions(mode)
        .into_iter()
        .filter_map(|value| {
            value
                .get("name")
                .and_then(|name| name.as_str())
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn mutating_tools_are_hidden_outside_auto() {
    let runtime = Runtime::new();
    let readonly = runtime.tools_for_mode(OperatingMode::ReadOnly);
    let plan = runtime.tools_for_mode(OperatingMode::Plan);
    let auto = runtime.tools_for_mode(OperatingMode::Auto);

    for tools in [readonly, plan] {
        assert!(tools.contains(&"read"));
        assert!(tools.contains(&"list"));
        assert!(tools.contains(&"search"));
        assert!(!tools.contains(&"shell"));
        assert!(!tools.contains(&"write"));
        assert!(!tools.contains(&"patch"));
    }

    assert!(auto.contains(&"shell"));
    assert!(auto.contains(&"write"));
    assert!(auto.contains(&"patch"));
}

#[test]
fn auto_without_code_intel_does_not_advertise_code_intel() {
    let runtime = Runtime::new();
    let auto = advertised_names(&runtime, OperatingMode::Auto);
    assert!(!auto.contains(&"code_intel".into()));
}

#[test]
fn auto_with_code_intel_advertises_code_intel() {
    let mut runtime = Runtime::new();
    runtime.set_code_intelligence(Arc::new(StubCodeIntel));
    let auto = advertised_names(&runtime, OperatingMode::Auto);
    assert!(auto.contains(&"code_intel".into()));
}

#[test]
fn plan_does_not_advertise_ask_question_when_interactive() {
    let mut runtime = Runtime::new();
    let (route, _responder) = slim_core::interaction_route();
    runtime.set_interaction_route(route);
    let plan = advertised_names(&runtime, OperatingMode::Plan);
    assert!(plan.contains(&"read".into()));
    assert!(!plan.contains(&"ask_question".into()));
    assert!(!plan.contains(&"write".into()));
    let readonly = advertised_names(&runtime, OperatingMode::ReadOnly);
    assert!(!readonly.contains(&"ask_question".into()));
    let auto = advertised_names(&runtime, OperatingMode::Auto);
    assert!(auto.contains(&"ask_question".into()));
}
