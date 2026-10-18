mod discovery;
mod invocation;
mod metadata;

use std::path::Path;

pub use discovery::{discover, discover_workspace, DiscoveryResult, SkillEntry, SkillRoot};
pub use invocation::{
    invoke_script, invoke_script_with_limits, launch_path_for_powershell, resolve_script_path,
    validate_invocation, SkillInvocationError, DEFAULT_SKILL_OUTPUT_BYTES, DEFAULT_SKILL_TIMEOUT,
};
pub use metadata::{read_body, read_metadata, SkillMetadata};

/// Script a skill runs when none is named: `run.ps1` on Windows, `run.sh`
/// elsewhere (`invocation` picks the interpreter by extension).
pub const DEFAULT_SKILL_SCRIPT: &str = if cfg!(windows) { "run.ps1" } else { "run.sh" };

/// Default script name after treating blank / `./` prefixes as
/// [`DEFAULT_SKILL_SCRIPT`].
pub fn default_skill_script(script: Option<&str>) -> &str {
    let script = script
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SKILL_SCRIPT);
    let stripped = script
        .strip_prefix("./")
        .or_else(|| script.strip_prefix(".\\"))
        .unwrap_or(script);
    if stripped.is_empty() {
        DEFAULT_SKILL_SCRIPT
    } else {
        stripped
    }
}

/// `SKILL.md` body when the default runner is missing. Instruction-only skills.
pub fn fallback_skill_body(skill_dir: impl AsRef<Path>, script: &str) -> Option<String> {
    if script != DEFAULT_SKILL_SCRIPT {
        return None;
    }
    let skill_dir = skill_dir.as_ref();
    if skill_dir.join(DEFAULT_SKILL_SCRIPT).is_file() {
        return None;
    }
    read_body(skill_dir.join("SKILL.md"))
        .ok()
        .filter(|body| !body.trim().is_empty())
}
