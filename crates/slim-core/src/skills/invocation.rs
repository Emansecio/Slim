use std::io;
use std::path::Path;

use crate::OperatingMode;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkillInvocationError {
    ModeDenied,
    TrustRequired,
    Io(String),
}

pub fn validate_invocation(mode: OperatingMode, trusted: bool) -> Result<(), SkillInvocationError> {
    if mode != OperatingMode::Auto {
        return Err(SkillInvocationError::ModeDenied);
    }
    if !trusted {
        return Err(SkillInvocationError::TrustRequired);
    }
    Ok(())
}

pub fn invoke_script(
    skill_dir: impl AsRef<Path>,
    script: &str,
    mode: OperatingMode,
    trusted: bool,
) -> Result<std::process::Output, SkillInvocationError> {
    validate_invocation(mode, trusted)?;
    let path = skill_dir.as_ref().join(script);
    let shell = if which("pwsh") { "pwsh" } else { "powershell" };
    std::process::Command::new(shell)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
        .arg(path)
        .current_dir(skill_dir)
        .output()
        .map_err(|error| SkillInvocationError::Io(error.to_string()))
}

fn which(command: &str) -> bool {
    std::process::Command::new("where.exe")
        .arg(command)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[allow(dead_code)]
fn _io_error(error: io::Error) -> SkillInvocationError {
    SkillInvocationError::Io(error.to_string())
}
