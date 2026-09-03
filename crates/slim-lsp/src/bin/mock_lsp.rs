//! Deterministic stdio LSP used only by integration tests. It is a real child
//! process so framing, process lifecycle and protocol ordering exercise the
//! same code paths as production language servers.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct Arguments {
    log: Option<PathBuf>,
    initialize_delay: Duration,
}

#[derive(Default)]
struct MockConfig {
    log_path: Option<PathBuf>,
    request_delay: Duration,
    definition_uri: Option<String>,
    publish_diagnostics: bool,
    diagnostic_version_delta: i64,
    document_symbol_nested: bool,
}

struct Logger {
    writer: Option<BufWriter<File>>,
}

impl Logger {
    fn new(path: Option<PathBuf>) -> io::Result<Self> {
        let writer = match path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                Some(BufWriter::new(File::create(path)?))
            }
            None => None,
        };
        Ok(Self { writer })
    }

    fn write(&mut self, value: &Value) -> io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        serde_json::to_writer(&mut *writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()
    }
}

fn main() {
    if let Err(error) = run() {
        let _ = writeln!(io::stderr(), "mock LSP failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = parse_arguments()?;
    let mut logger = Logger::new(arguments.log)?;
    logger.write(&json!({
        "event": "mock_startup",
        "cwd": std::env::current_dir()?.to_string_lossy(),
    }))?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let mut config = MockConfig::default();
    let mut shutdown_requested = false;

    while let Some(message) = read_frame(&mut reader)? {
        logger.write(&json!({ "direction": "client_to_server", "message": message }))?;
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => {
                config = MockConfig::from_initialize(&params);
                if let Some(log_path) = config.log_path.take() {
                    logger = Logger::new(Some(log_path))?;
                }
                if !arguments.initialize_delay.is_zero() {
                    thread::sleep(arguments.initialize_delay);
                }
                respond(
                    &mut writer,
                    &mut logger,
                    id,
                    json!({
                        "capabilities": {
                            "positionEncoding": "utf-16",
                            "textDocumentSync": 1,
                            "definitionProvider": true,
                            "referencesProvider": true,
                            "hoverProvider": true,
                            "documentSymbolProvider": true,
                            "workspaceSymbolProvider": true
                        },
                        "serverInfo": { "name": "slim-lsp-mock", "version": "1" }
                    }),
                )?;
            }
            "initialized" => {}
            "textDocument/didOpen" | "textDocument/didChange" => {
                if config.publish_diagnostics {
                    publish_diagnostics(&mut writer, &mut logger, &params, &config)?;
                }
            }
            "textDocument/didSave" | "textDocument/didClose" => {}
            "textDocument/hover" => {
                delay(config.request_delay);
                respond(
                    &mut writer,
                    &mut logger,
                    id,
                    json!({
                        "contents": { "kind": "plaintext", "value": "mock hover" }
                    }),
                )?;
            }
            "textDocument/definition" => {
                delay(config.request_delay);
                let uri = config
                    .definition_uri
                    .clone()
                    .or_else(|| {
                        params
                            .pointer("/textDocument/uri")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "file:///missing.rs".to_owned());
                respond(
                    &mut writer,
                    &mut logger,
                    id,
                    json!({
                        "uri": uri,
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 1 }
                        }
                    }),
                )?;
            }
            "textDocument/references" => {
                delay(config.request_delay);
                let uri = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .unwrap_or("file:///missing.rs");
                respond(
                    &mut writer,
                    &mut logger,
                    id,
                    json!([{
                        "uri": uri,
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 1 }
                        }
                    }]),
                )?;
            }
            "textDocument/documentSymbol" => {
                delay(config.request_delay);
                if config.document_symbol_nested {
                    respond(
                        &mut writer,
                        &mut logger,
                        id,
                        json!([{
                            "name": "outer",
                            "kind": 5,
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 4, "character": 1 }
                            },
                            "selectionRange": {
                                "start": { "line": 0, "character": 3 },
                                "end": { "line": 0, "character": 8 }
                            },
                            "children": [{
                                "name": "inner",
                                "kind": 12,
                                "range": {
                                    "start": { "line": 0, "character": 5 },
                                    "end": { "line": 0, "character": 10 }
                                },
                                "selectionRange": {
                                    "start": { "line": 0, "character": 6 },
                                    "end": { "line": 0, "character": 11 }
                                }
                            }]
                        }]),
                    )?;
                } else {
                    respond(
                        &mut writer,
                        &mut logger,
                        id,
                        json!([{
                            "name": "main",
                            "kind": 12,
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 2 }
                            },
                            "selectionRange": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 1 }
                            }
                        }]),
                    )?;
                }
            }
            "workspace/symbol" => {
                delay(config.request_delay);
                respond(&mut writer, &mut logger, id, json!([]))?;
            }
            "mock/barrier" => respond(&mut writer, &mut logger, id, Value::Null)?,
            "mock/beginIndexing" => {
                notify(
                    &mut writer,
                    &mut logger,
                    "$/progress",
                    json!({
                        "token": "rustAnalyzer/Roots Scanned",
                        "value": { "kind": "begin", "title": "Roots Scanned" }
                    }),
                )?;
                respond(&mut writer, &mut logger, id, Value::Null)?;
            }            "mock/flood" => {
                let count = params.get("count").and_then(Value::as_u64).unwrap_or(128);
                let uri = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or("file:///workspace/main.rs");
                for version in 0..count {
                    notify(
                        &mut writer,
                        &mut logger,
                        "textDocument/publishDiagnostics",
                        json!({
                            "uri": uri,
                            "version": version,
                            "diagnostics": []
                        }),
                    )?;
                }
                respond(&mut writer, &mut logger, id, json!(count))?;
            }
            "mock/publishDiagnostics" => {
                notify(
                    &mut writer,
                    &mut logger,
                    "textDocument/publishDiagnostics",
                    params,
                )?;
                respond(&mut writer, &mut logger, id, Value::Null)?;
            }
            "shutdown" => {
                shutdown_requested = true;
                respond(&mut writer, &mut logger, id, Value::Null)?;
            }
            "exit" => break,
            "$/cancelRequest" => {}
            _ if id.is_some() => respond(&mut writer, &mut logger, id, Value::Null)?,
            _ => {}
        }
    }

    logger.write(&json!({
        "event": "process_exit",
        "shutdown_requested": shutdown_requested
    }))?;
    Ok(())
}

impl MockConfig {
    fn from_initialize(params: &Value) -> Self {
        let mock = params
            .get("initializationOptions")
            .and_then(|value| value.get("mock"));
        Self {
            log_path: mock
                .and_then(|value| value.get("logPath"))
                .and_then(Value::as_str)
                .map(PathBuf::from),
            request_delay: Duration::from_millis(
                mock.and_then(|value| value.get("requestDelayMs"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ),
            definition_uri: mock
                .and_then(|value| value.get("definitionUri"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            publish_diagnostics: mock
                .and_then(|value| value.get("publishDiagnostics"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            diagnostic_version_delta: mock
                .and_then(|value| value.get("diagnosticVersionDelta"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
            document_symbol_nested: mock
                .and_then(|value| value.get("documentSymbolNested"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

fn parse_arguments() -> Result<Arguments, Box<dyn std::error::Error>> {
    let mut parsed = Arguments::default();
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--log" => {
                parsed.log = Some(PathBuf::from(arguments.next().ok_or("--log needs a path")?));
            }
            "--initialize-delay-ms" => {
                let value = arguments
                    .next()
                    .ok_or("--initialize-delay-ms needs a value")?
                    .to_string_lossy()
                    .parse::<u64>()?;
                parsed.initialize_delay = Duration::from_millis(value);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(parsed)
}

fn delay(duration: Duration) {
    if !duration.is_zero() {
        thread::sleep(duration);
    }
}

fn publish_diagnostics<W: Write>(
    writer: &mut W,
    logger: &mut Logger,
    params: &Value,
    config: &MockConfig,
) -> io::Result<()> {
    let uri = params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .unwrap_or("file:///missing.rs");
    let version = params
        .pointer("/textDocument/version")
        .and_then(Value::as_i64)
        .unwrap_or(1)
        .saturating_add(config.diagnostic_version_delta);
    notify(
        writer,
        logger,
        "textDocument/publishDiagnostics",
        json!({
            "uri": uri,
            "version": version,
            "diagnostics": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 1 }
                },
                "severity": 2,
                "source": "slim-lsp-mock",
                "message": format!("diagnostics v{version}")
            }]
        }),
    )
}

fn respond<W: Write>(
    writer: &mut W,
    logger: &mut Logger,
    id: Option<Value>,
    result: Value,
) -> io::Result<()> {
    let message = json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "result": result
    });
    write_logged(writer, logger, &message)
}

fn notify<W: Write>(
    writer: &mut W,
    logger: &mut Logger,
    method: &str,
    params: Value,
) -> io::Result<()> {
    let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    write_logged(writer, logger, &message)
}

fn write_logged<W: Write>(writer: &mut W, logger: &mut Logger, message: &Value) -> io::Result<()> {
    write_frame(writer, message)?;
    logger.write(&json!({ "direction": "server_to_client", "message": message }))
}

fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut header = Vec::with_capacity(128);
    let mut byte = [0_u8; 1];
    loop {
        let read = reader.read(&mut byte)?;
        if read == 0 {
            return Ok(None);
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LSP header exceeds 4096 bytes",
            ));
        }
    }
    let header = String::from_utf8_lossy(&header);
    let length = header
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?
        .1
        .trim()
        .parse::<usize>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP frame exceeds test limit",
        ));
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_frame<W: Write>(writer: &mut W, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}
