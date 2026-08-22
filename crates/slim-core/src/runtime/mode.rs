pub fn mode_name(mode: crate::OperatingMode) -> &'static str {
    match mode {
        crate::OperatingMode::Auto => "Auto",
        crate::OperatingMode::ReadOnly => "Read-only",
        crate::OperatingMode::Plan => "Plan",
    }
}
