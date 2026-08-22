//! PTY E2E on Windows ConPTY (DESIGN-SLIM-TUI §28.4 subset, gate C5):
//! spawn the real `slim --tui` binary in a pseudo-terminal, drive it with
//! keystrokes, and prove alternate-screen enter/echo/restore. A watchdog
//! kills the child if the graceful path stalls, so the test itself never
//! hangs the suite.

use portable_pty::native_pty_system;
use portable_pty::{CommandBuilder, PtySize};
use std::io::Write;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

fn spawn_reader(
    mut reader: Box<dyn std::io::Read + Send>,
) -> Receiver<u8> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for byte in &buf[..n] {
                        if tx.send(*byte).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// Collect output for at most `millis`.
fn drain(rx: &Receiver<u8>, millis: u64) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_millis(millis);
    let mut out = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(byte) => out.push(byte),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    out
}

#[test]
#[cfg(windows)]
// Sandbox/CI hosts may lack the interactive console stack: ConPTY opens but
// conhost produces no output (even `cmd /C echo`). Run on a real Windows
// console session with: cargo test -p slim-cli --test tui_pty -- --ignored
#[ignore = "requires a real ConPTY host; sandboxed sessions emit no conhost output"]
fn pty_full_session_enters_alt_screen_echos_and_restores() {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let exe = env!("CARGO_BIN_EXE_slim").to_string();
    let mut command = CommandBuilder::new(exe);
    command.args(["--tui"]);
    let auth = std::env::temp_dir().join(format!("slim-pty-auth-{}.json", std::process::id()));
    command.env("SLIM_AUTH_FILE", &auth);
    command.env("SLIM_API_KEY", "");

    let mut child = pair.slave.spawn_command(command).expect("spawn slim --tui");
    let reader_rx = spawn_reader(pair.master.try_clone_reader().expect("clone reader"));
    let mut writer = pair.master.take_writer().expect("take writer");

    // ConPTY itself probes with \x1b[6n at startup; poll until the app's
    // alternate-screen enter arrives.
    let mut boot = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        boot.extend(drain(&reader_rx, 250));
        if String::from_utf8_lossy(&boot).contains("1049h") {
            break;
        }
    }
    let early_status = child.try_wait().ok().flatten();
    let entered: String = boot.iter().map(|byte| format!("\\x{byte:02x}")).collect();
    assert!(
        entered.contains("1049h"),
        "alternate screen must be entered; early_exit={early_status:?}; got: {entered}"
    );

    // Composer echoes typed input; poll until it shows up.
    writer.write_all(b"hello from pty").expect("type");
    let mut echoed_bytes = Vec::new();
    let echo_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < echo_deadline {
        echoed_bytes.extend(drain(&reader_rx, 250));
        if String::from_utf8_lossy(&echoed_bytes).contains("hello from pty") {
            break;
        }
    }
    let echoed = String::from_utf8_lossy(&echoed_bytes);
    assert!(
        echoed.contains("hello from pty"),
        "composer must echo typed input, got: {echoed:?}"
    );

    // Clear the draft, then Ctrl+C on an idle empty composer exits cleanly
    // (spec §17.2). Watchdog kills if the graceful path stalls.
    for _ in 0..14 {
        writer.write_all(&[0x7f]).expect("backspace");
    }
    writer.flush().ok();
    thread::sleep(Duration::from_millis(200));
    writer.write_all(&[0x03]).expect("ctrl+c");
    writer.flush().ok();

    let mut exited_cleanly = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(_status)) = child.try_wait() {
            exited_cleanly = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !exited_cleanly {
        let _ = child.kill();
        let _ = child.wait();
    }

    // Restore must be observable either way: explicit shutdown emits 1049l,
    // RAII fallback also runs through TerminalGuard::restore/Drop.
    let tail = drain(&reader_rx, 1000);
    let restored = String::from_utf8_lossy(&tail)
        .to_string()
        + &String::from_utf8_lossy(&boot);
    assert!(
        restored.contains("\x1b[?1049l"),
        "alternate screen must be left on shutdown"
    );
    assert!(
        exited_cleanly,
        "idle Ctrl+C with empty draft must shut down gracefully"
    );
    let _ = std::fs::remove_file(auth);
}
