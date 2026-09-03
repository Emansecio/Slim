use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::process::{ProcessOutputBudget, ProcessRequest, ProcessRunner};
use crate::runtime::CancellationToken;
use crate::OperatingMode;

pub const DEFAULT_SKILL_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_SKILL_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkillInvocationError {
    ModeDenied,
    TrustRequired,
    InvalidScriptPath,
    ScriptOutsideSkill,
    Cancelled,
    TimedOut,
    OutputTooLarge,
    Io(String),
}

impl fmt::Display for SkillInvocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModeDenied => formatter.write_str("skill unavailable outside Auto mode"),
            Self::TrustRequired => formatter.write_str("skill requires user trust"),
            Self::InvalidScriptPath | Self::ScriptOutsideSkill => {
                formatter.write_str("skill script path is invalid")
            }
            Self::Cancelled => formatter.write_str("skill cancelled"),
            Self::TimedOut => formatter.write_str("skill timed out"),
            Self::OutputTooLarge => formatter.write_str("skill output exceeded the bound"),
            Self::Io(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SkillInvocationError {}

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
    invoke_script_with_limits(
        skill_dir,
        script,
        mode,
        trusted,
        DEFAULT_SKILL_TIMEOUT,
        DEFAULT_SKILL_OUTPUT_BYTES,
        None,
    )
}

pub fn invoke_script_with_limits(
    skill_dir: impl AsRef<Path>,
    script: &str,
    mode: OperatingMode,
    trusted: bool,
    timeout: Duration,
    max_output_bytes: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<std::process::Output, SkillInvocationError> {
    let skill_dir = skill_dir.as_ref();
    invoke_script_with_limits_and_runner(
        &ProcessRunner::default(),
        SkillInvocationRequest {
            skill_dir,
            script,
            mode,
            trusted,
            timeout,
            max_output_bytes,
            cancellation,
        },
    )
}

pub(crate) struct SkillInvocationRequest<'a> {
    pub skill_dir: &'a Path,
    pub script: &'a str,
    pub mode: OperatingMode,
    pub trusted: bool,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub cancellation: Option<&'a CancellationToken>,
}

pub(crate) fn invoke_script_with_limits_and_runner(
    runner: &ProcessRunner,
    request: SkillInvocationRequest<'_>,
) -> Result<std::process::Output, SkillInvocationError> {
    validate_invocation(request.mode, request.trusted)?;
    if request
        .cancellation
        .is_some_and(CancellationToken::is_cancelled)
    {
        return Err(SkillInvocationError::Cancelled);
    }
    let (path, canonical_dir) =
        resolve_script_path_with_dir(request.skill_dir, request.script)?;
    let program = runner
        .resolve_powershell()
        .map_err(|error| SkillInvocationError::Io(error.to_string()))?
        .ok_or_else(|| {
            SkillInvocationError::Io("neither pwsh nor powershell was found on PATH".into())
        })?;
    let process_request = ProcessRequest {
        cwd: launch_path_for_powershell(&canonical_dir),
        program,
        args: ["-NoLogo", "-NoProfile", "-NonInteractive", "-File"]
            .into_iter()
            .map(OsString::from)
            .chain(std::iter::once(
                launch_path_for_powershell(&path).into_os_string(),
            ))
            .collect(),
        timeout: request.timeout,
        cancellation: request.cancellation.cloned(),
        output_budget: ProcessOutputBudget::per_stream(request.max_output_bytes.saturating_add(1)),
    };
    let output = runner
        .run(process_request)
        .map_err(|error| SkillInvocationError::Io(error.to_string()))?;
    if output.cancelled {
        return Err(SkillInvocationError::Cancelled);
    }
    if output.timed_out {
        return Err(SkillInvocationError::TimedOut);
    }
    if output.stdout_discarded_bytes > 0
        || output.stderr_discarded_bytes > 0
        || output.output.stdout.len() > request.max_output_bytes
        || output.output.stderr.len() > request.max_output_bytes
    {
        return Err(SkillInvocationError::OutputTooLarge);
    }
    Ok(output.output)
}

/// Resolves a skill script without allowing an absolute path, a parent
/// traversal, or a reparse/symlink escape. Canonicalization happens before
/// process creation so the launched path is the same path that was checked.
/// Same as [`resolve_script_path`], additionally returning the canonical
/// skill directory so callers do not canonicalize it a second time.
pub(crate) fn resolve_script_path_with_dir(
    skill_dir: impl AsRef<Path>,
    script: &str,
) -> Result<(PathBuf, PathBuf), SkillInvocationError> {
    let script_path = Path::new(script);
    if script.trim().is_empty() || script_path.is_absolute() {
        return Err(SkillInvocationError::InvalidScriptPath);
    }
    let mut relative = PathBuf::new();
    for component in script_path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => relative.push(part),
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(SkillInvocationError::InvalidScriptPath);
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(SkillInvocationError::InvalidScriptPath);
    }
    let canonical_dir = skill_dir
        .as_ref()
        .canonicalize()
        .map_err(|error| SkillInvocationError::Io(error.to_string()))?;
    let canonical_script = canonical_dir
        .join(relative)
        .canonicalize()
        .map_err(|error| SkillInvocationError::Io(error.to_string()))?;
    if !canonical_script.starts_with(&canonical_dir) {
        return Err(SkillInvocationError::ScriptOutsideSkill);
    }
    Ok((canonical_script, canonical_dir))
}

pub fn resolve_script_path(
    skill_dir: impl AsRef<Path>,
    script: &str,
) -> Result<PathBuf, SkillInvocationError> {
    resolve_script_path_with_dir(skill_dir, script).map(|(script, _)| script)
}

/// Path passed to `powershell -File`. Jail checks keep the canonical
/// `\\?\` form; PowerShell RemoteSigned treats that prefix as remote.
pub fn launch_path_for_powershell(canonical: &Path) -> PathBuf {
    let text = canonical.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix("UNC\\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        return PathBuf::from(rest);
    }
    canonical.to_path_buf()
}
