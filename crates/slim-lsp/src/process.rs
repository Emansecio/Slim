//! Process plumbing for language servers: the stdin/stdout pair used as an
//! LspIo, the bounded stderr ring buffer, and best-effort process-tree kill
//! (rust-analyzer can spawn cargo and build-script children, so killing only
//! the direct child is not enough).

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::sync::Mutex;

/// Maximum bytes of server stderr kept for diagnostics (ring buffer).
pub const STDERR_TAIL_BYTES: usize = 16 * 1024;

/// Combines the child stdin (write) and stdout (read) into a single LspIo.
pub struct StdioPair {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

impl AsyncRead for StdioPair {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for StdioPair {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

/// Reads stderr until EOF, keeping only the tail in a ring buffer.
pub async fn capture_stderr_tail(
    mut stderr: tokio::process::ChildStderr,
    tail: Arc<Mutex<String>>,
) {
    let mut chunk = [0u8; 4096];
    loop {
        let read = match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let mut guard = tail.lock().await;
        let text = String::from_utf8_lossy(&chunk[..read]).into_owned();
        if guard.len() + text.len() > STDERR_TAIL_BYTES {
            // Keep the newest tail: drop the oldest bytes.
            let keep = STDERR_TAIL_BYTES.saturating_sub(text.len());
            let trimmed = trim_to_char_boundary(&guard, keep);
            let mut kept = guard.split_off(trimmed);
            kept.push_str(&text);
            *guard = kept;
        } else {
            guard.push_str(&text);
        }
    }
}

fn trim_to_char_boundary(text: &str, max_bytes: usize) -> usize {
    let mut at = max_bytes.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Best-effort recursive process tree termination.
#[cfg(windows)]
pub fn kill_process_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();
}

#[cfg(not(windows))]
pub fn kill_process_tree(pid: u32) {
    // Negative PID targets the process group the server was spawned in (see
    // StdioProcessFactory::spawn); fall back to the bare PID for processes
    // that are not group leaders.
    let group = std::process::Command::new("kill")
        .args(["-9", &format!("-{pid}")])
        .output();
    if !group.is_ok_and(|output| output.status.success()) {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    }
}

/// Appends a bounded tail of stderr (used by tests and status tooling).
pub fn bound_stderr_tail(text: &str) -> String {
    let mut out = text.to_owned();
    let at = trim_to_char_boundary(&out, STDERR_TAIL_BYTES);
    if at < out.len() {
        out = out.split_off(at);
        if !out.is_empty() && !out.starts_with('\n') {
            out.insert(0, '\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_respects_char_boundaries() {
        let text = "\u{e1}\u{e1}\u{e1}abcdef";
        let at = trim_to_char_boundary(text, 5);
        assert!(text.is_char_boundary(at));
        assert!(at <= 5);
    }

    #[test]
    fn bound_tail_keeps_newest_bytes() {
        let long = "x".repeat(20_000);
        let bounded = bound_stderr_tail(&long);
        assert!(bounded.len() <= STDERR_TAIL_BYTES);
        assert!(bounded.ends_with('x'));
    }
}
