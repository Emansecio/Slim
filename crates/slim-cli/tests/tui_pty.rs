//! Windows ConPTY gate for the deployed Slim TUI (DESIGN-SLIM-TUI §28.4).
//!
//! The test is deliberately ignored because some sandboxed Windows hosts can
//! open ConPTY while conhost emits no bytes. Run it explicitly with:
//!
//! `cargo test -p slim-cli --test tui_pty -- --ignored --nocapture`

use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, PtySize};
use serde_json::json;
use sha2::{Digest, Sha256};
use slim_cli::{run_provider_tui_turn, ProviderRequest, ProviderRunOptions};
use slim_core::provider::ProviderKind;
use slim_core::OperatingMode;
use slim_tui::api::UiEvent;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SENTINEL_KEY: &str = "SLIM_E2E_SENTINEL_KEY";
const PROMPT: &str = "OFFLINE_E2E_PROMPT inspect both fixture files";
const THINK_BEFORE: &str = "THINK_BEFORE_TOOLS";
const THINK_AFTER: &str = "THINK_AFTER_TOOLS";
const CALL_ONE_OUTPUT: &str = "CALL_ONE_OUTPUT";
const CALL_TWO_OUTPUT: &str = "CALL_TWO_OUTPUT";
const FINAL_ANSWER: &str = "FINAL_ANSWER_MARKER";
const CALL_ALPHA: &str = "homonymous-call-alpha";
const CALL_BETA: &str = "homonymous-call-beta";
const PHASE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy)]
struct MatrixCase {
    name: &'static str,
    cols: u16,
    rows: u16,
    reduced_motion: bool,
    no_color: bool,
    expand_tools: bool,
}

const MATRIX: [MatrixCase; 5] = [
    MatrixCase {
        name: "120x30-normal",
        cols: 120,
        rows: 30,
        reduced_motion: false,
        no_color: false,
        expand_tools: false,
    },
    MatrixCase {
        name: "80x24-reduced",
        cols: 80,
        rows: 24,
        reduced_motion: true,
        no_color: false,
        expand_tools: false,
    },
    MatrixCase {
        name: "60x16-no-color",
        cols: 60,
        rows: 16,
        reduced_motion: false,
        no_color: true,
        expand_tools: false,
    },
    MatrixCase {
        name: "40x10-reduced-no-color",
        cols: 40,
        rows: 10,
        reduced_motion: true,
        no_color: true,
        expand_tools: false,
    },
    MatrixCase {
        name: "32x10-detail",
        cols: 32,
        rows: 10,
        reduced_motion: true,
        no_color: true,
        expand_tools: true,
    },
];

#[derive(Clone)]
struct ExecutableIdentity {
    path: PathBuf,
    bytes: u64,
    sha256: String,
    source: &'static str,
}

impl ExecutableIdentity {
    fn description(&self) -> String {
        format!(
            "source={} path={} bytes={} sha256={}",
            self.source,
            self.path.display(),
            self.bytes,
            self.sha256
        )
    }
}

struct WorkspaceGuard {
    root: PathBuf,
}

impl WorkspaceGuard {
    fn new(case: MatrixCase) -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "slim-conpty-{}-{}-{nonce}",
            std::process::id(),
            case.name
        ));
        fs::create_dir_all(&root)
            .map_err(|error| format!("create fixture workspace {}: {error}", root.display()))?;
        Ok(Self { root })
    }
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ChildGuard {
    child: Box<dyn Child + Send>,
    reaped: bool,
}

impl ChildGuard {
    fn new(child: Box<dyn Child + Send>) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, String> {
        let status = self
            .child
            .try_wait()
            .map_err(|error| format!("poll child: {error}"))?;
        self.reaped |= status.is_some();
        Ok(status)
    }

    fn kill_and_reap(&mut self) {
        if self.reaped {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped = true;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

struct FixtureServer {
    endpoint: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<Result<usize, String>>>,
}

impl FixtureServer {
    fn finish(mut self) -> Result<usize, String> {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .expect("fixture worker")
            .join()
            .map_err(|_| "fixture server panicked".to_owned())?
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn resolve_e2e_executable() -> Result<ExecutableIdentity, String> {
    let (requested, source) = match std::env::var_os("SLIM_E2E_EXE") {
        Some(path) if !path.is_empty() => (PathBuf::from(path), "SLIM_E2E_EXE"),
        _ => (
            PathBuf::from(env!("CARGO_BIN_EXE_slim")),
            "CARGO_BIN_EXE_slim",
        ),
    };
    let path = fs::canonicalize(&requested).map_err(|error| {
        format!(
            "resolve executable source={source} requested={}: {error}",
            requested.display()
        )
    })?;
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("metadata for executable {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "resolved executable is not a file: {}",
            path.display()
        ));
    }
    let bytes = fs::read(&path)
        .map_err(|error| format!("read executable for hashing {}: {error}", path.display()))?;
    let sha256 = format!("{:X}", Sha256::digest(&bytes));
    Ok(ExecutableIdentity {
        path,
        bytes: metadata.len(),
        sha256,
        source,
    })
}

fn evidence_dir() -> Result<PathBuf, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = fs::canonicalize(&root)
        .map_err(|error| format!("resolve repository root {}: {error}", root.display()))?;
    let directory = root.join("analysis_outputs/streaming-thinking-tools-tui/conpty");
    fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "create ConPTY evidence directory {}: {error}",
            directory.display()
        )
    })?;
    Ok(directory)
}

fn spawn_reader(mut reader: Box<dyn Read + Send>) -> Receiver<u8> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(size) => {
                    for byte in &buffer[..size] {
                        if tx.send(*byte).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

fn drain_available(receiver: &Receiver<u8>, output: &mut Vec<u8>) {
    output.extend(receiver.try_iter());
}

fn wait_for_text(receiver: &Receiver<u8>, output: &mut Vec<u8>, needle: &str) -> bool {
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        if normalized_vt(output).contains(needle) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            drain_available(receiver, output);
            return normalized_vt(output).contains(needle);
        }
        let wait = (deadline - now).min(Duration::from_millis(100));
        match receiver.recv_timeout(wait) {
            Ok(byte) => output.push(byte),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                drain_available(receiver, output);
                return normalized_vt(output).contains(needle);
            }
        }
    }
}

fn drain_for(receiver: &Receiver<u8>, output: &mut Vec<u8>, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(byte) => output.push(byte),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drain_available(receiver, output);
}

fn drain_until_idle(
    receiver: &Receiver<u8>,
    output: &mut Vec<u8>,
    maximum: Duration,
    idle: Duration,
) {
    let deadline = Instant::now() + maximum;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining.min(idle)) {
            Ok(byte) => output.push(byte),
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drain_available(receiver, output);
}

fn read_http_request(stream: &mut TcpStream) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("set fixture read timeout: {error}"))?;
    let mut request = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut expected = None;
    loop {
        let size = stream
            .read(&mut buffer)
            .map_err(|error| format!("read provider request: {error}"))?;
        if size == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..size]);
        if request.len() > 2 * 1024 * 1024 {
            return Err("provider request exceeded 2 MiB fixture limit".into());
        }
        if expected.is_none() {
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                expected = Some(header_end.saturating_add(content_length));
            }
        }
        if expected.is_some_and(|length| request.len() >= length) {
            break;
        }
    }
    String::from_utf8(request).map_err(|error| format!("provider request was not UTF-8: {error}"))
}

fn write_sse(stream: &mut TcpStream, payload: &str) -> Result<(), String> {
    stream
        .write_all(format!("data: {payload}\n\n").as_bytes())
        .map_err(|error| format!("write fixture SSE: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("flush fixture SSE: {error}"))
}

fn accept_with_watchdog(
    listener: &TcpListener,
    stop: &AtomicBool,
) -> Result<Option<TcpStream>, String> {
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(None);
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .map_err(|error| format!("set fixture stream blocking: {error}"))?;
                return Ok(Some(stream));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err("fixture timed out waiting for provider connection".into());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("accept fixture connection: {error}")),
        }
    }
}

fn start_fixture(path_one: PathBuf, path_two: PathBuf) -> Result<FixtureServer, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("bind loopback provider: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set loopback provider nonblocking: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("read loopback provider address: {error}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = thread::spawn(move || {
        let mut requests = 0usize;
        for turn in 0..2 {
            let Some(mut stream) = accept_with_watchdog(&listener, &worker_stop)? else {
                return Ok(requests);
            };
            let request = read_http_request(&mut stream)
                .map_err(|error| format!("read provider request (turn {turn}): {error}"))?;
            let normalized_headers = request.to_ascii_lowercase();
            if !normalized_headers
                .contains(&format!("authorization: bearer {SENTINEL_KEY}").to_ascii_lowercase())
            {
                return Err(format!(
                    "provider request did not use the sentinel credential (turn {turn}, {} bytes)",
                    request.len()
                ));
            }
            if turn == 0 && !request.contains(PROMPT) {
                return Err(format!(
                    "first provider request omitted the typed prompt ({} bytes)",
                    request.len()
                ));
            }
            if turn == 1
                && (!request.contains(CALL_ONE_OUTPUT) || !request.contains(CALL_TWO_OUTPUT))
            {
                return Err("second provider request omitted a homonymous tool result".into());
            }
            requests += 1;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .map_err(|error| format!("write fixture headers: {error}"))?;

            if turn == 0 {
                write_sse(
                    &mut stream,
                    &json!({"choices":[{"delta":{"reasoning_content":THINK_BEFORE}}]}).to_string(),
                )?;
                thread::sleep(Duration::from_millis(12));
                let tool_calls = json!({
                    "choices": [{"delta": {"tool_calls": [
                        {
                            "index": 0,
                            "id": CALL_ALPHA,
                            "function": {
                                "name": "read",
                                "arguments": json!({"path": path_one, "max_lines": 4096}).to_string()
                            }
                        },
                        {
                            "index": 1,
                            "id": CALL_BETA,
                            "function": {
                                "name": "read",
                                "arguments": json!({"path": path_two, "max_lines": 4096}).to_string()
                            }
                        }
                    ]}}]
                });
                write_sse(&mut stream, &tool_calls.to_string())?;
                thread::sleep(Duration::from_millis(12));
                write_sse(
                    &mut stream,
                    r#"{"usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
                )?;
                write_sse(
                    &mut stream,
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                )?;
                write_sse(&mut stream, "[DONE]")?;
            } else {
                write_sse(
                    &mut stream,
                    &json!({"choices":[{"delta":{"reasoning_content":THINK_AFTER}}]}).to_string(),
                )?;
                thread::sleep(Duration::from_millis(12));
                write_sse(
                    &mut stream,
                    &json!({"choices":[{"delta":{"content":FINAL_ANSWER}}]}).to_string(),
                )?;
                write_sse(
                    &mut stream,
                    r#"{"usage":{"prompt_tokens":23,"completion_tokens":13}}"#,
                )?;
                write_sse(
                    &mut stream,
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                )?;
                write_sse(&mut stream, "[DONE]")?;
            }
        }
        Ok(requests)
    });
    Ok(FixtureServer {
        endpoint: format!("http://{address}"),
        stop,
        worker: Some(worker),
    })
}

fn fixture_text(marker: &str) -> String {
    let mut output = format!("{marker}\n");
    // About 25 KiB: large enough to force the 16 KiB content pager while the
    // two tool results still fit the fixture model's context budget.
    for index in 0..400 {
        output.push_str(&format!("{marker}-line-{index:04}-{}\n", "x".repeat(32)));
    }
    output
}

fn normalized_vt(raw: &[u8]) -> String {
    let mut visible = Vec::with_capacity(raw.len());
    let mut index = 0usize;
    while index < raw.len() {
        match raw[index] {
            0x1b if raw.get(index + 1) == Some(&b'[') => {
                index += 2;
                while index < raw.len() {
                    let byte = raw[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            0x1b if raw.get(index + 1) == Some(&b']') => {
                index += 2;
                while index < raw.len() {
                    if raw[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if raw[index] == 0x1b && raw.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            0x1b => index = (index + 2).min(raw.len()),
            b'\r' => {
                visible.push(b'\n');
                index += 1;
            }
            b'\n' | b'\t' => {
                visible.push(raw[index]);
                index += 1;
            }
            byte if byte >= 0x20 => {
                visible.push(byte);
                index += 1;
            }
            _ => index += 1,
        }
    }
    String::from_utf8_lossy(&visible).into_owned()
}

fn sanitized_capture(raw: &[u8], workspace: &Path, auth: &Path) -> Vec<u8> {
    String::from_utf8_lossy(raw)
        .replace(SENTINEL_KEY, "[REDACTED_KEY]")
        .replace(workspace.to_string_lossy().as_ref(), "[WORKSPACE]")
        .replace(auth.to_string_lossy().as_ref(), "[AUTH_FILE]")
        .into_bytes()
}

fn normalized_diff(normalized: &str) -> String {
    let mut previous = String::new();
    let mut output = String::new();
    for line in normalized
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if line == previous {
            continue;
        }
        output.push_str("+ ");
        output.push_str(line);
        output.push('\n');
        previous.clear();
        previous.push_str(line);
    }
    output
}

fn persist_capture(
    directory: &Path,
    identity: &ExecutableIdentity,
    case: MatrixCase,
    raw: &[u8],
    workspace: &Path,
    auth: &Path,
    outcome: &Result<(), String>,
) -> Result<(usize, usize), String> {
    let sanitized = sanitized_capture(raw, workspace, auth);
    let normalized = normalized_vt(&sanitized);
    if String::from_utf8_lossy(&sanitized).contains(SENTINEL_KEY)
        || normalized.contains(workspace.to_string_lossy().as_ref())
        || normalized.contains(auth.to_string_lossy().as_ref())
    {
        return Err("capture sanitizer retained fixture-sensitive material".into());
    }
    fs::write(directory.join(format!("{}.vt", case.name)), &sanitized)
        .map_err(|error| format!("write raw capture for {}: {error}", case.name))?;
    let status = match outcome {
        Ok(()) => "PASS".to_owned(),
        Err(error) => format!("FAIL: {error}"),
    };
    let text = format!(
        "case={}\nsize={}x{}\nreduced_motion={}\nno_color={}\n{}\noutcome={}\n\n--- normalized visible update stream ---\n{}\n--- normalized update diff ---\n{}",
        case.name,
        case.cols,
        case.rows,
        case.reduced_motion,
        case.no_color,
        identity.description(),
        status,
        normalized,
        normalized_diff(&normalized)
    );
    fs::write(directory.join(format!("{}.txt", case.name)), text)
        .map_err(|error| format!("write normalized capture for {}: {error}", case.name))?;
    Ok((sanitized.len(), normalized.len()))
}

fn wait_for_exit(
    child: &mut ChildGuard,
    receiver: &Receiver<u8>,
    raw: &mut Vec<u8>,
) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + PHASE_TIMEOUT;
    loop {
        drain_available(receiver, raw);
        if let Some(status) = child.try_wait()? {
            drain_for(receiver, raw, Duration::from_millis(250));
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("child did not exit before watchdog deadline".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_causal_stream(normalized: &str) -> Result<(), String> {
    let markers = [
        THINK_BEFORE,
        CALL_ONE_OUTPUT,
        CALL_TWO_OUTPUT,
        THINK_AFTER,
        FINAL_ANSWER,
    ];
    let mut previous = 0usize;
    for (index, marker) in markers.iter().enumerate() {
        let position = normalized
            .find(marker)
            .ok_or_else(|| format!("missing causal marker {marker}"))?;
        if index > 0 && position <= previous {
            return Err(format!("causal marker {marker} rendered out of order"));
        }
        previous = position;
    }
    Ok(())
}

fn drive_case(
    identity: &ExecutableIdentity,
    evidence: &Path,
    case: MatrixCase,
) -> Result<(usize, usize), String> {
    let workspace = WorkspaceGuard::new(case)?;
    let path_one = workspace.root.join("CALL_ONE_FILE.txt");
    let path_two = workspace.root.join("CALL_TWO_FILE.txt");
    fs::write(&path_one, fixture_text(CALL_ONE_OUTPUT))
        .map_err(|error| format!("write first tool fixture: {error}"))?;
    fs::write(&path_two, fixture_text(CALL_TWO_OUTPUT))
        .map_err(|error| format!("write second tool fixture: {error}"))?;
    let fixture = start_fixture(
        PathBuf::from("CALL_ONE_FILE.txt"),
        PathBuf::from("CALL_TWO_FILE.txt"),
    )?;
    let auth = workspace.root.join("auth-does-not-exist.json");
    let mut raw = Vec::new();

    let outcome = (|| -> Result<(), String> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: case.rows,
                cols: case.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("open ConPTY {}x{}: {error}", case.cols, case.rows))?;
        let mut command = CommandBuilder::new(&identity.path);
        command.args([
            "--tui",
            "--provider",
            "openai-compatible",
            "--endpoint",
            &fixture.endpoint,
            "--model",
            "offline-fixture-model",
        ]);
        command.cwd(&workspace.root);
        command.env("SLIM_API_KEY", SENTINEL_KEY);
        command.env("SLIM_AUTH_FILE", &auth);
        command.env("NO_PROXY", "127.0.0.1,localhost");
        command.env_remove("NO_COLOR");
        command.env_remove("SLIM_REDUCED_MOTION");
        if case.no_color {
            command.env("NO_COLOR", "1");
        }
        if case.reduced_motion {
            command.env("SLIM_REDUCED_MOTION", "1");
        }

        let child = pair.slave.spawn_command(command).map_err(|error| {
            format!(
                "spawn TUI failed ({}) case={}: {error}",
                identity.description(),
                case.name
            )
        })?;
        let mut child = ChildGuard::new(child);
        let receiver = spawn_reader(
            pair.master
                .try_clone_reader()
                .map_err(|error| format!("clone ConPTY reader: {error}"))?,
        );
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|error| format!("take ConPTY writer: {error}"))?;

        if !wait_for_text(&receiver, &mut raw, "SLIM") {
            let early = child.try_wait()?;
            return Err(format!(
                "startup emitted no rendered SLIM frame; early_exit={early:?}; {}",
                identity.description()
            ));
        }
        if !String::from_utf8_lossy(&raw).contains("1049h") {
            return Err("alternate screen enter (1049h) was not observed".into());
        }

        writer
            .write_all(PROMPT.as_bytes())
            .map_err(|error| format!("type prompt: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("flush prompt: {error}"))?;
        if !wait_for_text(&receiver, &mut raw, PROMPT) {
            return Err("composer did not echo the typed prompt".into());
        }
        writer
            .write_all(b"\r")
            .map_err(|error| format!("submit prompt: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("flush submit: {error}"))?;
        if !wait_for_text(&receiver, &mut raw, FINAL_ANSWER) {
            return Err("offline provider did not reach the final answer marker".into());
        }

        let normalized = normalized_vt(&raw);
        assert_causal_stream(&normalized)?;

        if case.expand_tools {
            writer
                .write_all(b"\x1b[H")
                .map_err(|error| format!("navigate Home: {error}"))?;
            writer
                .flush()
                .map_err(|error| format!("flush Home navigation: {error}"))?;
            drain_until_idle(
                &receiver,
                &mut raw,
                Duration::from_millis(500),
                Duration::from_millis(40),
            );
            let mut expanded = false;
            for _ in 0..48 {
                writer
                    .write_all(b"\x1b[B")
                    .map_err(|error| format!("navigate selection Down: {error}"))?;
                writer
                    .flush()
                    .map_err(|error| format!("flush selection Down: {error}"))?;
                drain_until_idle(
                    &receiver,
                    &mut raw,
                    Duration::from_millis(500),
                    Duration::from_millis(40),
                );
                writer
                    .write_all(b"\r")
                    .map_err(|error| format!("activate selected block: {error}"))?;
                writer
                    .flush()
                    .map_err(|error| format!("flush selected block: {error}"))?;
                drain_until_idle(
                    &receiver,
                    &mut raw,
                    Duration::from_millis(500),
                    Duration::from_millis(40),
                );
                let visible = normalized_vt(&raw);
                if visible.contains(CALL_ALPHA) && visible.contains(CALL_BETA) {
                    expanded = true;
                    break;
                }
            }
            if !expanded {
                return Err(
                    "selection/Enter did not expand the homonymous tool group by call ID".into(),
                );
            }
        }

        writer
            .write_all(&[0x03])
            .map_err(|error| format!("send idle Ctrl+C: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("flush idle Ctrl+C: {error}"))?;
        let status = wait_for_exit(&mut child, &receiver, &mut raw)?;
        if !status.success() {
            return Err(format!("TUI exited unsuccessfully: {status}"));
        }
        if !String::from_utf8_lossy(&raw).contains("\x1b[?1049l") {
            return Err("alternate screen restore (1049l) was not observed".into());
        }
        Ok(())
    })();

    let fixture_result = fixture.finish();
    let outcome = outcome.and_then(|()| {
        let requests = fixture_result?;
        if requests != 2 {
            return Err(format!("fixture observed {requests} requests instead of 2"));
        }
        Ok(())
    });
    let sizes = persist_capture(
        evidence,
        identity,
        case,
        &raw,
        &workspace.root,
        &auth,
        &outcome,
    )?;
    outcome.map(|()| sizes)
}

#[test]
fn loopback_fixture_exercises_reasoning_homonymous_tools_and_answer_offline() {
    let case = MATRIX[0];
    let workspace = WorkspaceGuard::new(case).expect("offline fixture workspace");
    let path_one = workspace.root.join("CALL_ONE_FILE.txt");
    let path_two = workspace.root.join("CALL_TWO_FILE.txt");
    fs::write(&path_one, fixture_text(CALL_ONE_OUTPUT)).expect("first offline fixture");
    fs::write(&path_two, fixture_text(CALL_TWO_OUTPUT)).expect("second offline fixture");
    let fixture = start_fixture(
        PathBuf::from("CALL_ONE_FILE.txt"),
        PathBuf::from("CALL_TWO_FILE.txt"),
    )
    .expect("start offline fixture");
    let endpoint = fixture.endpoint.clone();

    let events = match run_provider_tui_turn(
        ProviderRequest {
            prompt: PROMPT.into(),
            mode: OperatingMode::Auto,
            kind: ProviderKind::OpenAiCompatible,
            endpoint,
            model: "offline-fixture-model".into(),
            api_key: SENTINEL_KEY.into(),
            account_id: None,
            timeout: PHASE_TIMEOUT,
        },
        ProviderRunOptions::default()
            .with_workspace_root(&workspace.root)
            .with_artifact_root(workspace.root.join("artifacts")),
    ) {
        Ok(events) => events,
        Err(error) => {
            let fixture_result = fixture.finish();
            panic!("offline fixture turn: {error:?}; fixture={fixture_result:?}");
        }
    };
    assert_eq!(fixture.finish().expect("finish offline fixture"), 2);

    let thinking = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ThinkingDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(thinking, format!("{THINK_BEFORE}{THINK_AFTER}"));
    let mut reasoning_open = false;
    let mut reasoning_starts = 0;
    let mut reasoning_ends = 0;
    for event in &events {
        match event {
            UiEvent::ThinkingStarted => {
                assert!(!reasoning_open, "reasoning cannot start twice");
                reasoning_open = true;
                reasoning_starts += 1;
            }
            UiEvent::ThinkingDelta { .. } => {
                assert!(reasoning_open, "reasoning delta must be inside lifecycle");
            }
            UiEvent::ThinkingEnded => {
                assert!(reasoning_open, "reasoning cannot end before start");
                reasoning_open = false;
                reasoning_ends += 1;
            }
            _ => {}
        }
    }
    assert!(!reasoning_open, "reasoning lifecycle must close");
    assert_eq!((reasoning_starts, reasoning_ends), (2, 2));
    let call_ids = events
        .iter()
        .filter_map(|event| match event {
            UiEvent::ToolStarted { call_id, .. } => Some(call_id.0.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(call_ids.len(), 2);
    assert!(call_ids[0].ends_with(CALL_ALPHA));
    assert!(call_ids[1].ends_with(CALL_BETA));
    assert_ne!(call_ids[0], call_ids[1]);
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolProgress { preview, .. } if preview.contains(CALL_ONE_OUTPUT)
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolProgress { preview, .. } if preview.contains(CALL_TWO_OUTPUT)
    )));
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                UiEvent::AssistantDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>(),
        FINAL_ANSWER
    );
}

#[test]
fn vt_normalization_and_sanitization_preserve_evidence_without_secrets() {
    let workspace = Path::new(r"C:\temp\fixture");
    let auth = workspace.join("auth.json");
    let raw = format!(
        "\x1b[?1049h\x1b]0;secret title\x07visible 界 {SENTINEL_KEY} {} {}\r\n\x1b[?1049l",
        workspace.display(),
        auth.display()
    );
    let sanitized = sanitized_capture(raw.as_bytes(), workspace, &auth);
    let normalized = normalized_vt(&sanitized);
    assert!(normalized.contains("visible 界 [REDACTED_KEY] [WORKSPACE]"));
    assert!(!normalized.contains(SENTINEL_KEY));
    assert!(!normalized.contains(workspace.to_string_lossy().as_ref()));
    assert!(!normalized.contains("secret title"));
    assert!(String::from_utf8_lossy(&sanitized).contains("\x1b[?1049h"));
    assert!(String::from_utf8_lossy(&sanitized).contains("\x1b[?1049l"));
}

#[test]
fn conpty_matrix_freezes_required_dimensions_and_modes() {
    assert_eq!(
        MATRIX
            .iter()
            .map(|case| (case.cols, case.rows))
            .collect::<Vec<_>>(),
        [(120, 30), (80, 24), (60, 16), (40, 10), (32, 10)]
    );
    assert!(MATRIX
        .iter()
        .any(|case| !case.reduced_motion && !case.no_color));
    assert!(MATRIX.iter().any(|case| case.reduced_motion));
    assert!(MATRIX.iter().any(|case| case.no_color));
    assert_eq!(MATRIX.iter().filter(|case| case.expand_tools).count(), 1);
}

#[test]
#[cfg(windows)]
#[ignore = "requires a real ConPTY host; sandboxed sessions may emit no conhost output"]
fn deployed_binary_offline_streaming_matrix() {
    let identity = resolve_e2e_executable().unwrap_or_else(|error| panic!("{error}"));
    let evidence = evidence_dir().unwrap_or_else(|error| panic!("{error}"));
    let mut summary = format!(
        "# ConPTY offline matrix\n\n- executable: `{}`\n- bytes: {}\n- SHA-256: `{}`\n- source: `{}`\n- provider: in-process loopback only\n- credential: sentinel, never persisted\n\n| case | size | reduced | no color | raw bytes | normalized bytes | outcome |\n|---|---:|:---:|:---:|---:|---:|---|\n",
        identity.path.display(),
        identity.bytes,
        identity.sha256,
        identity.source
    );

    for case in MATRIX {
        match drive_case(&identity, &evidence, case) {
            Ok((raw_bytes, normalized_bytes)) => {
                summary.push_str(&format!(
                    "| {} | {}x{} | {} | {} | {} | {} | PASS |\n",
                    case.name,
                    case.cols,
                    case.rows,
                    case.reduced_motion,
                    case.no_color,
                    raw_bytes,
                    normalized_bytes
                ));
            }
            Err(error) => {
                summary.push_str(&format!(
                    "| {} | {}x{} | {} | {} | - | - | FAIL: {} |\n",
                    case.name,
                    case.cols,
                    case.rows,
                    case.reduced_motion,
                    case.no_color,
                    error.replace('|', "\\|")
                ));
                fs::write(evidence.join("matrix.md"), &summary)
                    .unwrap_or_else(|write_error| panic!("write matrix summary: {write_error}"));
                panic!("ConPTY case {} failed: {error}", case.name);
            }
        }
    }
    fs::write(evidence.join("matrix.md"), &summary)
        .unwrap_or_else(|error| panic!("write matrix summary: {error}"));
}
