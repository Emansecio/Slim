use slim_core::runtime::Runtime;
use slim_core::OperatingMode;

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
