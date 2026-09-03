use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use super::ToolError;
use crate::process::{ProcessOutputBudget, ProcessProgress, ProcessRequest, ProcessRunner};
use crate::runtime::CancellationToken;

const SHELL_CAPTURE_CAP_BYTES: usize = 8 * 1024 * 1024;

pub fn run_shell(cwd: impl AsRef<Path>, command: &str) -> Result<Output, ToolError> {
    let runner = ProcessRunner::default();
    let result = run_with_runner(
        &runner,
        cwd.as_ref(),
        command,
        Duration::MAX,
        None,
        ProcessOutputBudget::per_stream(usize::MAX),
        |_| {},
    )?;
    Ok(result.output)
}

#[derive(Debug)]
pub struct TimedShellOutput {
    pub output: Output,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout_discarded_bytes: usize,
    pub stderr_discarded_bytes: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShellProgress {
    pub elapsed_ms: u64,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub last_line: String,
}

impl From<ProcessProgress> for ShellProgress {
    fn from(progress: ProcessProgress) -> Self {
        Self {
            elapsed_ms: progress.elapsed_ms,
            stdout_bytes: progress.stdout_bytes,
            stderr_bytes: progress.stderr_bytes,
            last_line: progress.last_line,
        }
    }
}

pub fn run_shell_timeout(
    cwd: impl AsRef<Path>,
    command: &str,
    timeout: Duration,
) -> Result<TimedShellOutput, ToolError> {
    run_shell_timeout_cancellable(cwd, command, timeout, None)
}

pub fn run_shell_timeout_cancellable(
    cwd: impl AsRef<Path>,
    command: &str,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
) -> Result<TimedShellOutput, ToolError> {
    run_shell_timeout_cancellable_with_progress(cwd, command, timeout, cancellation, |_| {})
}

pub fn run_shell_timeout_cancellable_with_progress(
    cwd: impl AsRef<Path>,
    command: &str,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    on_progress: impl FnMut(ShellProgress),
) -> Result<TimedShellOutput, ToolError> {
    run_shell_timeout_cancellable_with_progress_and_runner(
        &ProcessRunner::default(),
        cwd,
        command,
        timeout,
        cancellation,
        on_progress,
    )
}

pub(crate) fn run_shell_timeout_cancellable_with_progress_and_runner(
    runner: &ProcessRunner,
    cwd: impl AsRef<Path>,
    command: &str,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    mut on_progress: impl FnMut(ShellProgress),
) -> Result<TimedShellOutput, ToolError> {
    run_with_runner(
        runner,
        cwd.as_ref(),
        command,
        timeout,
        cancellation,
        ProcessOutputBudget::per_stream(SHELL_CAPTURE_CAP_BYTES),
        |progress| on_progress(progress.into()),
    )
}

fn run_with_runner(
    runner: &ProcessRunner,
    cwd: &Path,
    command: &str,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    output_budget: ProcessOutputBudget,
    on_progress: impl FnMut(ProcessProgress),
) -> Result<TimedShellOutput, ToolError> {
    if command.trim().is_empty() {
        return Err(ToolError::InvalidInput {
            message: "shell command cannot be empty".into(),
        });
    }
    let (program, args) = shell_invocation(runner, command)?;
    let request = ProcessRequest {
        cwd: cwd.to_path_buf(),
        program,
        args,
        timeout,
        cancellation: cancellation.cloned(),
        output_budget,
    };
    let result = runner.run_with_progress(request, on_progress)?;
    Ok(TimedShellOutput {
        output: result.output,
        timed_out: result.timed_out,
        cancelled: result.cancelled,
        stdout_discarded_bytes: result.stdout_discarded_bytes,
        stderr_discarded_bytes: result.stderr_discarded_bytes,
    })
}

fn shell_invocation(
    runner: &ProcessRunner,
    command: &str,
) -> Result<(PathBuf, Vec<OsString>), ToolError> {
    #[cfg(windows)]
    {
        if !looks_like_powershell(command) {
            let program = std::env::var_os("COMSPEC")
                .map(PathBuf::from)
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| PathBuf::from("cmd.exe"));
            return Ok((
                program,
                vec![
                    OsString::from("/d"),
                    OsString::from("/s"),
                    OsString::from("/c"),
                    OsString::from(command),
                ],
            ));
        }
    }
    let program = powershell_program(runner)?;
    Ok((
        program,
        [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
    ))
}

#[cfg(windows)]
fn looks_like_powershell(command: &str) -> bool {
    if command.contains('$') || command.contains("[Console]") || command.contains("::") {
        return true;
    }
    let lower = command.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "start-sleep",
        "write-output",
        "write-host",
        "set-content",
        "get-content",
        "test-path",
        "out-null",
        "foreach-object",
        "select-object",
        "where-object",
        "invoke-expression",
        " -eq ",
        " -ne ",
        " -match ",
        " -like ",
        " -not ",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

fn powershell_program(runner: &ProcessRunner) -> Result<PathBuf, ToolError> {
    runner.resolve_powershell()?.ok_or_else(|| ToolError::Io {
        message: "neither pwsh nor powershell was found on PATH".into(),
    })
}
