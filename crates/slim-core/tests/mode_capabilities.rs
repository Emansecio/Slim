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
fn mcp_meta_tool_only_in_auto_with_enabled_servers() {
    use slim_core::mcp::{McpManager, McpServerSpec, McpTransport};
    use slim_core::process::ExecutableResolver;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    fn spec(enabled: bool) -> McpServerSpec {
        McpServerSpec {
            name: "srv".into(),
            transport: McpTransport::Stdio {
                command: "cmd".into(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            enabled,
            timeout: Duration::from_millis(1_000),
        }
    }
    fn manager(enabled: bool) -> Arc<McpManager> {
        Arc::new(McpManager::new(
            BTreeMap::from([("srv".to_owned(), spec(enabled))]),
            PathBuf::from("."),
            ExecutableResolver::default(),
        ))
    }

    let bare = Runtime::new();
    assert!(!advertised_names(&bare, OperatingMode::Auto).contains(&"mcp".into()));

    let mut disabled = Runtime::new();
    disabled.set_mcp_manager(Some(manager(false)));
    assert!(!advertised_names(&disabled, OperatingMode::Auto).contains(&"mcp".into()));

    let mut enabled = Runtime::new();
    enabled.set_mcp_manager(Some(manager(true)));
    assert!(advertised_names(&enabled, OperatingMode::Auto).contains(&"mcp".into()));
    assert!(!advertised_names(&enabled, OperatingMode::ReadOnly).contains(&"mcp".into()));
    assert!(!advertised_names(&enabled, OperatingMode::Plan).contains(&"mcp".into()));
}

#[test]
fn plan_does_not_advertise_ask_question_when_interactive() {
    let mut runtime = Runtime::new();
    assert!(!advertised_names(&runtime, OperatingMode::ReadOnly).contains(&"ask_question".into()));
    let (route, _responder) = slim_core::interaction_route();
    runtime.set_interaction_route(route);
    let plan = advertised_names(&runtime, OperatingMode::Plan);
    assert!(plan.contains(&"read".into()));
    assert!(!plan.contains(&"ask_question".into()));
    assert!(!plan.contains(&"write".into()));
    let readonly = advertised_names(&runtime, OperatingMode::ReadOnly);
    assert!(readonly.contains(&"ask_question".into()));
    assert!(!readonly.contains(&"write".into()));
    let auto = advertised_names(&runtime, OperatingMode::Auto);
    assert!(auto.contains(&"ask_question".into()));
}
