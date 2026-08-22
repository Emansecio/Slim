use std::io::Read;
use std::path::Path;
use std::process::Output;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::ToolError;
use crate::runtime::CancellationToken;

pub fn run_shell(cwd: impl AsRef<Path>, command: &str) -> Result<Output, ToolError> {
    if command.trim().is_empty() {
        return Err(ToolError::InvalidInput {
            message: "shell command cannot be empty".into(),
        });
    }
    let shell = if which("pwsh") { "pwsh" } else { "powershell" };
    std::process::Command::new(shell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ])
        .current_dir(cwd)
        .output()
        .map_err(ToolError::from)
}

#[derive(Debug)]
pub struct TimedShellOutput {
    pub output: Output,
    pub timed_out: bool,
    pub cancelled: bool,
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
    if command.trim().is_empty() {
        return Err(ToolError::InvalidInput {
            message: "shell command cannot be empty".into(),
        });
    }
    let shell = if which("pwsh") { "pwsh" } else { "powershell" };
    let mut child = std::process::Command::new(shell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(ToolError::from)?;
    let stdout = child.stdout.take().ok_or_else(|| ToolError::Io {
        message: "shell stdout pipe is unavailable".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::Io {
        message: "shell stderr pipe is unavailable".into(),
    })?;
    let stdout_reader = std::thread::spawn(move || read_pipe(stdout));
    let stderr_reader = std::thread::spawn(move || read_pipe(stderr));

    let start = Instant::now();
    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(ToolError::from)? {
            break status;
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            cancelled = true;
            terminate_process_tree(&mut child);
            break child.wait().map_err(ToolError::from)?;
        }
        if start.elapsed() >= timeout {
            timed_out = true;
            terminate_process_tree(&mut child);
            break child.wait().map_err(ToolError::from)?;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let output = Output {
        status,
        stdout: join_pipe(stdout_reader, "stdout")?,
        stderr: join_pipe(stderr_reader, "stderr")?,
    };
    Ok(TimedShellOutput {
        output,
        timed_out,
        cancelled,
    })
}

fn read_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_pipe(
    reader: JoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
) -> Result<Vec<u8>, ToolError> {
    reader
        .join()
        .map_err(|_| ToolError::Io {
            message: format!("shell {stream} reader panicked"),
        })?
        .map_err(ToolError::from)
}

fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill.exe")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
}

fn which(command: &str) -> bool {
    std::process::Command::new("where.exe")
        .arg(command)
        .output()
        .is_ok_and(|output| output.status.success())
}
