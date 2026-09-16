//! Diagnostic ConPTY probe: what does crossterm actually deliver to the app
//! when a terminal pastes text (with and without bracketed-paste markers)?
//!
//! The test re-executes its own binary under a ConPTY slave. The child reads
//! console input via `crossterm::event::read()` — the same entry point the
//! runtime uses — and appends every raw `Event`, the `more_input` flag and the
//! `PasteStreamDecoder` output to a log file. The parent writes paste payloads
//! to the master input exactly like Windows Terminal does, then the log is
//! printed so the real record stream (modifiers, kinds, gaps) is visible.
//!
//! Run explicitly:
//! `cargo test -p slim-tui --test paste_conpty_probe -- --ignored --nocapture`

#![cfg(windows)]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{poll, read};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use slim_tui::input::PasteStreamDecoder;

const CHILD_ENV: &str = "SLIM_PASTE_PROBE_LOG";

fn payloads() -> Vec<(&'static str, Vec<Vec<u8>>, u64)> {
    vec![
        (
            "bracketed-crlf",
            vec![b"\x1b[200~line one\r\nline two\x1b[201~".to_vec()],
            0,
        ),
        (
            "bracketed-lf",
            vec![b"\x1b[200~alpha\nbeta\ncharlie\x1b[201~".to_vec()],
            0,
        ),
        (
            "unmarked-crlf",
            vec![b"unmarked one\r\nunmarked two\r\n".to_vec()],
            0,
        ),
        ("tilde-chars", vec![b"a~b `c~d'".to_vec()], 0),
        (
            "split-before-enter",
            vec![
                b"first part".to_vec(),
                b"\r".to_vec(),
                b"\nsecond part".to_vec(),
            ],
            80,
        ),
    ]
}

fn run_probe_child(log: &Path) {
    let mut file = fs::File::create(log).expect("create probe log");
    writeln!(file, "BOOT pid={}", std::process::id()).expect("log boot");
    file.flush().expect("flush boot");
    match crossterm::terminal::enable_raw_mode() {
        Ok(()) => writeln!(file, "RAW-OK").expect("log raw"),
        Err(error) => writeln!(file, "RAW-ERR {error}").expect("log raw err"),
    }
    file.flush().expect("flush raw");
    let mut decoder = PasteStreamDecoder::default();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last_event = Instant::now();
    while Instant::now() < deadline {
        match poll(Duration::from_millis(100)) {
            Ok(true) => match read() {
                Ok(event) => {
                    let gap_us = last_event.elapsed().as_micros();
                    let more = poll(Duration::ZERO).unwrap_or(false);
                    writeln!(file, "EVENT {event:?} more_input={more} gap_us={gap_us}")
                        .expect("log event");
                    for decoded in decoder.feed(event.clone(), more) {
                        writeln!(file, "  DECODED {decoded:?}").expect("log decoded");
                    }
                    if !more {
                        for decoded in decoder.end_of_input() {
                            writeln!(file, "  DECODED-EOT {decoded:?}").expect("log decoded eot");
                        }
                    }
                    file.flush().expect("flush probe log");
                    last_event = Instant::now();
                }
                Err(error) => {
                    writeln!(file, "READ-ERR {error}").expect("log error");
                    break;
                }
            },
            Ok(false) => {
                if last_event.elapsed() > Duration::from_secs(15) {
                    break;
                }
            }
            Err(error) => {
                writeln!(file, "POLL-ERR {error}").expect("log poll error");
                break;
            }
        }
    }
    let _ = crossterm::terminal::disable_raw_mode();
}

fn wait_for_log_content(log: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let content = fs::read_to_string(log).unwrap_or_default();
        if content.contains(needle) || Instant::now() >= deadline {
            return content;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "diagnostic ConPTY probe; run explicitly with --nocapture"]
fn conpty_paste_event_probe() {
    if let Some(log) = std::env::var_os(CHILD_ENV) {
        run_probe_child(Path::new(&log));
        return;
    }

    let log = std::env::temp_dir().join(format!(
        "slim-paste-probe-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open ConPTY");
    let mut command = CommandBuilder::new(std::env::current_exe().expect("test exe"));
    command.args([
        "--ignored",
        "--exact",
        "conpty_paste_event_probe",
        "--nocapture",
        "--test-threads",
        "1",
    ]);
    command.env(CHILD_ENV, &log);
    command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn probe child");
    let mut writer = pair.master.take_writer().expect("ConPTY writer");
    let mut reader = pair.master.try_clone_reader().expect("ConPTY reader");
    let pty_out = log.with_extension("pty.log");
    let pty_out_thread = pty_out.clone();
    let (pty_tx, pty_rx) = std::sync::mpsc::channel::<u8>();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        let mut out = fs::File::create(pty_out_thread).expect("pty out log");
        loop {
            match std::io::Read::read(&mut reader, &mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(size) => {
                    let _ = out.write_all(&buffer[..size]);
                    let _ = out.flush();
                    for byte in &buffer[..size] {
                        if pty_tx.send(*byte).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });

    // portable-pty opens ConPTY with INHERIT_CURSOR: conhost blocks console
    // startup until the host answers the startup DSR query (`\x1b[6n`).
    {
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !seen.ends_with(b"\x1b[6n") {
            match pty_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(byte) => seen.push(byte),
                Err(_) => break,
            }
        }
        if seen.ends_with(b"\x1b[6n") {
            writer
                .write_all(b"\x1b[1;1R")
                .and_then(|()| writer.flush())
                .expect("answer ConPTY cursor query");
        }
    }

    // Let the child boot and enable raw mode before payloads arrive.
    std::thread::sleep(Duration::from_secs(2));
    eprintln!("child status after boot: {:?}", child.try_wait());

    for (name, chunks, gap_ms) in payloads() {
        for chunk in chunks {
            writer
                .write_all(&chunk)
                .and_then(|()| writer.flush())
                .unwrap_or_else(|error| panic!("write {name}: {error}"));
            if gap_ms > 0 {
                std::thread::sleep(Duration::from_millis(gap_ms));
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    std::thread::sleep(Duration::from_secs(16));
    let _ = child.kill();
    let _ = child.wait();

    let content = wait_for_log_content(&log, "EVENT", Duration::from_secs(2));
    println!("=== probe log: {} ===\n{content}", log.display());
    let pty_content = fs::read_to_string(&pty_out).unwrap_or_default();
    println!(
        "=== pty output ===\n{}",
        &pty_content[..pty_content.len().min(4096)]
    );
    let _ = fs::remove_file(&log);
    let _ = fs::remove_file(&pty_out);
    assert!(
        content.contains("EVENT"),
        "probe child produced no console events"
    );
}
