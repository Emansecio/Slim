use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;

use slim_cli::{run_cli, run_tui, ExitCode};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[test]
fn stdin_is_used_when_prompt_argument_is_absent() {
    let output = run_cli(["--fake", "--read-only"], "hello from stdin");
    assert_eq!(output.code, ExitCode::Success);
    assert_eq!(output.stdout, "success\n");
}

#[test]
fn headless_without_provider_is_auth_not_silent_success() {
    let output = run_cli(["--headless", "--prompt", "Reply exactly READY."], "");
    assert_eq!(output.code, ExitCode::Auth);
    assert!(
        output.stderr.contains("--fake") || output.stderr.contains("provider"),
        "must name how to connect or opt into fake: {}",
        output.stderr
    );
    assert_ne!(output.stdout, "success\n");
}

#[test]
fn headless_binary_reads_piped_stdin_without_prompt() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_slim"))
        .args(["--headless", "--fake", "--read-only"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn headless");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"hello from stdin")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait");
    assert_eq!(output.status.code(), Some(ExitCode::Success.as_i32()));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "success\n");
}

#[test]
fn headless_binary_rejects_stdin_above_the_prompt_budget() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_slim"))
        .args(["--headless", "--fake", "--read-only"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn headless");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&vec![b'x'; 8 * 1024 * 1024 + 1])
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait");

    assert_eq!(output.status.code(), Some(ExitCode::InputRequired.as_i32()));
    assert!(String::from_utf8_lossy(&output.stderr).contains("stdin exceeds"));
}

#[test]
fn headless_codex_without_jwt_account_id_is_auth() {
    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock");
    let previous_slim = std::env::var_os("SLIM_API_KEY");
    let previous_codex = std::env::var_os("CODEX_ACCESS_TOKEN");
    std::env::remove_var("SLIM_API_KEY");
    std::env::set_var("CODEX_ACCESS_TOKEN", "dummy-offline-token");
    let output = run_cli(
        ["--headless", "--provider", "openai-codex", "--prompt", "hi"],
        "",
    );
    if let Some(value) = previous_slim {
        std::env::set_var("SLIM_API_KEY", value);
    } else {
        std::env::remove_var("SLIM_API_KEY");
    }
    if let Some(value) = previous_codex {
        std::env::set_var("CODEX_ACCESS_TOKEN", value);
    } else {
        std::env::remove_var("CODEX_ACCESS_TOKEN");
    }
    assert_eq!(output.code, ExitCode::Auth);
    assert!(
        output.stderr.contains("account id"),
        "auth message should name the missing account id: {}",
        output.stderr
    );
}

#[test]
fn headless_codex_oauth_store_is_not_missing_api_key() {
    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock");
    let root = std::env::temp_dir().join(format!(
        "slim-cli-oauth-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("temp auth dir");
    let path = root.join("auth.json");
    std::fs::write(
        &path,
        r#"{"version":1,"providers":{"openai-codex":{"oauth":{"access":"oauth-access","refresh":"oauth-refresh","expires":4102444800,"account_id":"acct-1"}}}}"#,
    )
    .expect("write oauth auth.json");
    let previous_auth = std::env::var_os("SLIM_AUTH_FILE");
    let previous_slim = std::env::var_os("SLIM_API_KEY");
    let previous_codex = std::env::var_os("CODEX_ACCESS_TOKEN");
    std::env::set_var("SLIM_AUTH_FILE", &path);
    std::env::remove_var("SLIM_API_KEY");
    std::env::remove_var("CODEX_ACCESS_TOKEN");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "fixture accept timeout"
                    );
                    thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        stream.set_nonblocking(false).expect("blocking stream");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("read timeout");
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0_u8; 4096];
            let size = stream.read(&mut chunk).expect("request");
            assert_ne!(size, 0, "request ended before the HTTP headers");
            request.extend_from_slice(&chunk[..size]);
            assert!(request.len() <= 32 * 1024, "request headers too large");
        }
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header terminator")
            + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-length: ")
                    .or_else(|| line.strip_prefix("Content-Length: "))
            })
            .map(|value| value.trim().parse::<usize>().expect("content length"))
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let mut chunk = [0_u8; 4096];
            let size = stream.read(&mut chunk).expect("request body");
            assert_ne!(size, 0, "request ended before the HTTP body");
            request.extend_from_slice(&chunk[..size]);
        }
        let request = String::from_utf8_lossy(&request);
        assert!(request.contains("oauth-access"), "OAuth token was not used");
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"oauth-ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("response");
    });
    let endpoint = format!("http://{address}");
    let output = run_cli(
        [
            "--headless",
            "--provider",
            "openai-codex",
            "--endpoint",
            endpoint.as_str(),
            "--prompt",
            "hi",
        ],
        "",
    );
    server.join().expect("server");
    match previous_auth {
        Some(value) => std::env::set_var("SLIM_AUTH_FILE", value),
        None => std::env::remove_var("SLIM_AUTH_FILE"),
    }
    match previous_slim {
        Some(value) => std::env::set_var("SLIM_API_KEY", value),
        None => std::env::remove_var("SLIM_API_KEY"),
    }
    match previous_codex {
        Some(value) => std::env::set_var("CODEX_ACCESS_TOKEN", value),
        None => std::env::remove_var("CODEX_ACCESS_TOKEN"),
    }
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(output.code, ExitCode::Success, "stderr={}", output.stderr);
    assert!(output.stdout.contains("oauth-ok"));
    assert!(
        !output.stderr.contains("no API key"),
        "oauth store must not report missing API key: {}",
        output.stderr
    );
}

#[test]
fn plan_and_jsonl_flags_are_executable_contracts() {
    let output = run_cli(
        ["--fake", "--plan", "--jsonl", "--prompt", "make a plan"],
        "",
    );
    assert_eq!(output.code, ExitCode::ApprovalRequired);
    assert_eq!(
        output.stdout,
        "{\"version\":1,\"kind\":\"approval_required\"}\n"
    );
}

#[test]
fn verbose_human_timeline_rejects_jsonl() {
    let output = run_cli(
        [
            "--headless",
            "--fake",
            "--verbose",
            "--jsonl",
            "--prompt",
            "hello",
        ],
        "",
    );
    assert_eq!(output.code, ExitCode::InputRequired);
    assert_eq!(
        output.stderr,
        "--verbose is available only with human text output; remove --jsonl\n"
    );
}

#[test]
fn binary_defaults_to_tui_and_headless_requires_its_flag() {
    let tui = Command::new(env!("CARGO_BIN_EXE_slim"))
        .args(["--provider", "unsupported"])
        .output()
        .expect("default tui");
    assert_eq!(tui.status.code(), Some(ExitCode::Provider.as_i32()));
    assert!(String::from_utf8_lossy(&tui.stderr).contains("tui error: unsupported provider"));

    let headless = Command::new(env!("CARGO_BIN_EXE_slim"))
        .args(["--headless", "--fake", "--read-only", "--prompt", "hello"])
        .output()
        .expect("headless");
    assert!(headless.status.success());
    assert_eq!(String::from_utf8_lossy(&headless.stdout), "success\n");
}

#[test]
fn tui_startup_preserves_provider_error_class() {
    let error = run_tui(vec![
        "--tui".into(),
        "--provider".into(),
        "unsupported".into(),
    ])
    .expect_err("unsupported provider");
    assert_eq!(error.code(), ExitCode::Provider);
}

#[test]
fn tui_help_uses_the_normal_cli_contract_without_opening_fullscreen() {
    let output = Command::new(env!("CARGO_BIN_EXE_slim"))
        .args(["--tui", "--help"])
        .output()
        .expect("binary help");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("Slim coding agent\n\nUsage:"));
}

#[test]
fn help_version_and_unknown_flags_are_stable() {
    assert_eq!(
        run_cli(["--help"], "").stdout,
        "Slim coding agent\n\nUsage:\n  Slim [TUI OPTIONS]\n  Slim --headless [OPTIONS] [PROMPT...]\n\nModes:\n  --tui              Open the fullscreen TUI (default)\n  --headless         Run one prompt without the TUI\n  --fake             Use the deterministic offline provider\n  --plan             Allow inspection without workspace mutations\n  --read-only        Disable workspace mutations\n\nProvider:\n  --provider NAME    Provider route\n  --model MODEL      Model identifier (Codex: astra, sol, terra, luna)\n  --effort LEVEL     Reasoning effort\n  --fast             Enable Codex Fast (higher usage)\n  --normal           Use normal Codex speed\n  --endpoint URL     Override the provider endpoint\n\nInput and sessions:\n  --prompt TEXT      Prompt text; positional text or stdin also works\n  --image PATH       Attach a local image (repeatable)\n  --session PATH     Persist the run to a session file\n  --resume PATH      Continue an existing session\n  --recover PATH     Repair a durable session without running a prompt\n  --abandon-pending  With --recover: abandon unfinished work; effects stay unverified\n\nOutput:\n  --verbose          Include detailed human-readable events\n  --jsonl            Emit machine-readable JSON Lines\n\nOther:\n  -h, --help         Show this help\n  -V, --version      Show the version\n\nExamples:\n  Slim\n  Slim --headless --fake \"Summarize this repository\"\n  Slim --headless --provider anthropic --model MODEL --prompt \"Review src\"\n"
    );
    assert_eq!(run_cli(["--version"], "").stdout, "slim 0.1.0\n");
    assert_eq!(run_cli(["--unknown"], "").code, ExitCode::Internal);
}
