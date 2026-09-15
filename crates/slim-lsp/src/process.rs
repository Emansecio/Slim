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
        push_tail(&mut guard, &String::from_utf8_lossy(&chunk[..read]));
    }
}

/// Appends `chunk` to `buf`, dropping whole chars from the front so the
/// buffer never exceeds STDERR_TAIL_BYTES. The newest bytes always survive.
fn push_tail(buf: &mut String, chunk: &str) {
    buf.push_str(chunk);
    if buf.len() > STDERR_TAIL_BYTES {
        *buf = buf.split_off(tail_start(buf, STDERR_TAIL_BYTES));
    }
}

/// First byte index of a suffix of `text` at most `max_bytes` long, rounded
/// up to a char boundary so a multibyte char is never split.
fn tail_start(text: &str, max_bytes: usize) -> usize {
    let mut start = text.len().saturating_sub(max_bytes);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    start
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
    if out.len() > STDERR_TAIL_BYTES {
        // One byte of headroom keeps the leading newline inside the bound.
        out = out.split_off(tail_start(&out, STDERR_TAIL_BYTES - 1));
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
    fn tail_start_respects_char_boundaries() {
        // á is 2 bytes; a cut inside it must move forward to the boundary.
        let text = "áááabcdef";
        let start = tail_start(text, 7);
        assert!(text.is_char_boundary(start));
        assert!(text.len() - start <= 7);
    }

    #[test]
    fn tail_start_keeps_max_suffix_bytes() {
        let text = "abcdefghij";
        assert_eq!(tail_start(text, 4), 6);
        assert_eq!(&text[6..], "ghij");
        // Inputs shorter than the limit start at zero.
        assert_eq!(tail_start(text, 10), 0);
        assert_eq!(tail_start(text, 64), 0);
    }

    #[test]
    fn push_tail_keeps_full_tail_not_just_newest_chunk() {
        // Regression for the inverted split: 16K old + 4K new must retain the
        // last 16K of the combined stream (12K old + 4K new), not only 8K.
        let mut buf = "o".repeat(STDERR_TAIL_BYTES);
        push_tail(&mut buf, &"n".repeat(4096));
        assert_eq!(buf.len(), STDERR_TAIL_BYTES);
        assert_eq!(buf.matches('o').count(), STDERR_TAIL_BYTES - 4096);
        assert!(buf.ends_with(&"n".repeat(4096)));
    }

    #[test]
    fn push_tail_bounds_oversized_single_chunk() {
        let mut buf = String::new();
        push_tail(&mut buf, &"z".repeat(STDERR_TAIL_BYTES + 500));
        assert_eq!(buf.len(), STDERR_TAIL_BYTES);
    }

    #[test]
    fn push_tail_never_splits_a_char() {
        // Full buffer of 2-byte chars + 1 new byte: the cut lands inside the
        // oldest á and must move forward, keeping one byte short of the cap.
        let mut buf = "á".repeat(STDERR_TAIL_BYTES / 2);
        push_tail(&mut buf, "b");
        assert_eq!(buf.len(), STDERR_TAIL_BYTES - 1);
        assert!(buf.ends_with('b'));
    }

    #[test]
    fn bound_tail_keeps_newest_bytes() {
        let long = "x".repeat(20_000);
        let bounded = bound_stderr_tail(&long);
        assert!(bounded.len() <= STDERR_TAIL_BYTES);
        assert!(bounded.ends_with('x'));
        // The kept suffix must be ~the full limit, not len - limit.
        assert!(bounded.len() >= STDERR_TAIL_BYTES - 1);
    }
}
