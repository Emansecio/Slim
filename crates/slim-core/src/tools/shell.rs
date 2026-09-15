use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use super::ToolError;
use crate::process::{
    ProcessExecutionFacts, ProcessOutputBudget, ProcessProgress, ProcessRequest, ProcessRunner,
};
use crate::runtime::CancellationToken;

const SHELL_CAPTURE_CAP_BYTES: usize = 8 * 1024 * 1024;

pub(crate) enum ShellInvocation<'a> {
    Script(&'a str),
    Program {
        executable: &'a str,
        args: &'a [String],
    },
}

pub fn run_shell(cwd: impl AsRef<Path>, command: &str) -> Result<Output, ToolError> {
    let runner = ProcessRunner::default();
    let result = run_with_runner(
        &runner,
        cwd.as_ref(),
        ShellInvocation::Script(command),
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

impl TimedShellOutput {
    pub(crate) fn execution_facts(&self) -> ProcessExecutionFacts {
        ProcessExecutionFacts::from_output(
            &self.output,
            self.timed_out,
            self.cancelled,
            self.stdout_discarded_bytes,
            self.stderr_discarded_bytes,
        )
    }
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
        ShellInvocation::Script(command),
        timeout,
        cancellation,
        on_progress,
    )
}

pub(crate) fn run_shell_timeout_cancellable_with_progress_and_runner(
    runner: &ProcessRunner,
    cwd: impl AsRef<Path>,
    invocation: ShellInvocation<'_>,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    on_progress: impl FnMut(ShellProgress),
) -> Result<TimedShellOutput, ToolError> {
    run_shell_timeout_cancellable_with_progress_and_runner_with_budget(
        runner,
        cwd,
        invocation,
        timeout,
        cancellation,
        ProcessOutputBudget::per_stream(SHELL_CAPTURE_CAP_BYTES),
        on_progress,
    )
}

pub(crate) fn run_shell_timeout_cancellable_with_progress_and_runner_with_budget(
    runner: &ProcessRunner,
    cwd: impl AsRef<Path>,
    invocation: ShellInvocation<'_>,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    output_budget: ProcessOutputBudget,
    mut on_progress: impl FnMut(ShellProgress),
) -> Result<TimedShellOutput, ToolError> {
    run_with_runner(
        runner,
        cwd.as_ref(),
        invocation,
        timeout,
        cancellation,
        output_budget,
        |progress| on_progress(progress.into()),
    )
}

fn run_with_runner(
    runner: &ProcessRunner,
    cwd: &Path,
    invocation: ShellInvocation<'_>,
    timeout: Duration,
    cancellation: Option<&CancellationToken>,
    output_budget: ProcessOutputBudget,
    on_progress: impl FnMut(ProcessProgress),
) -> Result<TimedShellOutput, ToolError> {
    let command = match &invocation {
        ShellInvocation::Script(command) => *command,
        ShellInvocation::Program { executable, .. } => *executable,
    };
    if command.trim().is_empty() {
        return Err(ToolError::InvalidInput {
            message: "shell command cannot be empty".into(),
        });
    }
    let (program, args) = match invocation {
        ShellInvocation::Script(command) => shell_invocation(runner, command)?,
        ShellInvocation::Program { executable, args } => {
            let path = Path::new(executable);
            let program = if path.is_absolute() || path.components().count() == 1 {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            (program, args.iter().map(OsString::from).collect())
        }
    };
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
    let program = powershell_program(runner)?;
    // PowerShell -Command otherwise collapses a native program's nonzero
    // exit code to 1. Preserve it without turning a failed cmdlet into success.
    let script = format!(
        "{command}\nif (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}; exit 1 }}"
    );
    Ok((
        program,
        [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
    ))
}

fn powershell_program(runner: &ProcessRunner) -> Result<PathBuf, ToolError> {
    runner.resolve_powershell()?.ok_or_else(|| ToolError::Io {
        message: "neither pwsh nor powershell was found on PATH".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ExecutableResolver;
    use std::fs;

    fn fixture_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("slim-shell-direct-{}-{name}", std::process::id()));
        fs::create_dir_all(&root).expect("root");
        root
    }

    fn write_executable(root: &Path, name: &str) -> PathBuf {
        let exe = root.join(if cfg!(windows) {
            format!("{name}.EXE")
        } else {
            name.to_owned()
        });
        fs::write(&exe, b"fixture").expect("exe");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("mode");
        }
        exe
    }

    fn runner_for(root: &Path) -> ProcessRunner {
        ProcessRunner::new(ExecutableResolver::with_environment(
            std::env::join_paths([root]).expect("PATH"),
            OsString::from(if cfg!(windows) { ".EXE" } else { "" }),
        ))
    }

    #[test]
    fn script_invocation_always_uses_powershell() {
        let root = fixture_root("script");
        let powershell = write_executable(&root, "pwsh");
        write_executable(&root, "fake");
        let runner = runner_for(&root);
        let (program, args) =
            shell_invocation(&runner, "fake --flag 'two words'").expect("resolve");

        assert_eq!(program, powershell);
        assert_eq!(args[0], OsString::from("-NoLogo"));
        assert_eq!(args[1], OsString::from("-NoProfile"));
        assert_eq!(args[2], OsString::from("-NonInteractive"));
        assert_eq!(args[3], OsString::from("-Command"));
        assert!(args[4]
            .to_string_lossy()
            .starts_with("fake --flag 'two words'"));
        let _ = fs::remove_dir_all(root);
    }
}
