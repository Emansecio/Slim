//! Deterministic stdio LSP used only by integration tests. It is a real child
//! process so framing, process lifecycle and protocol ordering exercise the
//! same code paths as production language servers.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use slim_lsp::{PositionCodec, PositionEncoding};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct Arguments {
    log: Option<PathBuf>,
    initialize_delay: Duration,
}

#[derive(Default)]
struct MockConfig {
    text_document_sync: Value,
    position_encoding: String,
    responses: serde_json::Map<String, Value>,
    log_path: Option<PathBuf>,
    request_delay: Duration,
    definition_uri: Option<String>,
    publish_diagnostics: bool,
    diagnostic_version_delta: i64,
    document_symbol_nested: bool,
    semantic_from_document: bool,
}

struct Logger {
    writer: Option<BufWriter<File>>,
    started: std::time::Instant,
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
        Ok(Self {
            writer,
            started: std::time::Instant::now(),
        })
    }

    fn write(&mut self, value: &Value) -> io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        #[derive(serde::Serialize)]
        struct TimedRow<'a> {
            elapsed_us: u128,
            #[serde(flatten)]
            value: &'a Value,
        }
        serde_json::to_writer(
            &mut *writer,
            &TimedRow {
                elapsed_us: self.started.elapsed().as_micros(),
                value,
            },
        )?;
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
    let mut documents = HashMap::<String, String>::new();
    let mut document_versions = HashMap::<String, i64>::new();
    let mut shutdown_requested = false;

    while let Some(message) = read_frame(&mut reader)? {
        logger.write(&json!({ "direction": "client_to_server", "message": message }))?;
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        if let Some(result) = config.responses.get(method).filter(|_| id.is_some()) {
            delay(config.request_delay);
            respond(&mut writer, &mut logger, id, result.clone())?;
            continue;
        }

        match method {
            "initialize" => {
                config = MockConfig::from_initialize(&params);
                if let Some(log_path) = config.log_path.take() {
                    logger.writer = Logger::new(Some(log_path))?.writer;
                    logger.write(&json!({"direction":"client_to_server", "message":message}))?;
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
                            "positionEncoding": config.position_encoding.clone(),
                            "textDocumentSync": config.text_document_sync,
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
            "initialized" => {
                // Mirror rust-analyzer: a completed Roots Scanned cycle right
                // after initialization, so indexing_observed is reachable.
                notify(
                    &mut writer,
                    &mut logger,
                    "$/progress",
                    json!({
                        "token": "rustAnalyzer/Roots Scanned",
                        "value": { "kind": "begin", "title": "Roots Scanned" }
                    }),
                )?;
                notify(
                    &mut writer,
                    &mut logger,
                    "$/progress",
                    json!({
                        "token": "rustAnalyzer/Roots Scanned",
                        "value": { "kind": "end" }
                    }),
                )?;
            }
            "textDocument/didOpen" => {
                let uri = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .ok_or("didOpen without URI")?
                    .to_owned();
                let text = params
                    .pointer("/textDocument/text")
                    .and_then(Value::as_str)
                    .ok_or("didOpen without text")?
                    .to_owned();
                let version = params
                    .pointer("/textDocument/version")
                    .and_then(Value::as_i64)
                    .unwrap_or(1);
                documents.insert(uri.clone(), text);
                document_versions.insert(uri, version);
                logger.write(&json!({"event":"didOpen_applied"}))?;
                if config.publish_diagnostics {
                    publish_diagnostics(&mut writer, &mut logger, &params, &config)?;
                }
            }
            "textDocument/didChange" => {
                let uri = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .ok_or("didChange without URI")?
                    .to_owned();
                let document = documents
                    .get_mut(&uri)
                    .ok_or_else(|| format!("didChange for unopened document {uri}"))?;
                for change in params
                    .get("contentChanges")
                    .and_then(Value::as_array)
                    .ok_or("didChange without contentChanges")?
                {
                    apply_content_change(document, change, &config.position_encoding)
                        .map_err(|error| format!("invalid didChange for {uri}: {error}"))?;
                }
                if let Some(version) = params
                    .pointer("/textDocument/version")
                    .and_then(Value::as_i64)
                {
                    document_versions.insert(uri.clone(), version);
                }
                if config.publish_diagnostics {
                    publish_diagnostics(&mut writer, &mut logger, &params, &config)?;
                }
            }
            "textDocument/didSave" | "textDocument/didClose" => {}
            "textDocument/hover" => {
                delay(config.request_delay);
                let uri = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let value = if config.semantic_from_document {
                    documents
                        .get(uri)
                        .map(|text| text.chars().take(600).collect::<String>())
                        .unwrap_or_default()
                } else {
                    "mock hover".to_owned()
                };
                respond(
                    &mut writer,
                    &mut logger,
                    id,
                    json!({
                        "contents": { "kind": "plaintext", "value": value }
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
            "mock/documentState" => {
                let uri = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                respond(
                    &mut writer,
                    &mut logger,
                    id,
                    json!({
                        "text": documents.get(uri),
                        "version": document_versions.get(uri),
                    }),
                )?;
            }
            "mock/crash" => {
                return Err(
                    io::Error::new(io::ErrorKind::BrokenPipe, "intentional mock crash").into(),
                );
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
            }
            "mock/flood" => {
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
            text_document_sync: mock
                .and_then(|value| value.get("textDocumentSync"))
                .cloned()
                .unwrap_or(json!(1)),
            position_encoding: mock
                .and_then(|value| value.get("positionEncoding"))
                .and_then(Value::as_str)
                .unwrap_or("utf-16")
                .to_owned(),
            responses: mock
                .and_then(|value| value.get("responses"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
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
            semantic_from_document: mock
                .and_then(|value| value.get("semanticFromDocument"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

fn apply_content_change(
    document: &mut String,
    change: &Value,
    encoding: &str,
) -> Result<(), String> {
    let replacement = change
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| "change text is not a string".to_owned())?;
    let Some(range) = change.get("range") else {
        replacement.clone_into(document);
        return Ok(());
    };
    let start = range_position(document, range.get("start"), encoding)?;
    let end = range_position(document, range.get("end"), encoding)?;
    if start > end {
        return Err("range start is after range end".to_owned());
    }
    document.replace_range(start..end, replacement);
    Ok(())
}

fn range_position(document: &str, value: Option<&Value>, encoding: &str) -> Result<usize, String> {
    let position = value.ok_or_else(|| "missing range position".to_owned())?;
    let line = position
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| "range line is not an integer".to_owned())? as usize;
    let character = position
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| "range character is not an integer".to_owned())?
        as usize;
    let mut line_start = 0;
    let mut current_line = 0;
    for (index, byte) in document.bytes().enumerate() {
        if current_line == line {
            break;
        }
        if byte == b'\n' {
            current_line += 1;
            line_start = index + 1;
        }
    }
    if current_line != line {
        return Err(format!("line {line} is outside the document"));
    }
    let line_end = document[line_start..]
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(document.len());
    let line_text = &document[line_start..line_end];
    let position_encoding = match encoding.to_ascii_lowercase().as_str() {
        "utf-8" => PositionEncoding::Utf8,
        "utf-32" => PositionEncoding::Utf32,
        _ => PositionEncoding::Utf16,
    };
    PositionCodec::character_to_byte(position_encoding, line_text, character as u32)
        .map(|offset| line_start + offset)
        .ok_or_else(|| format!("character {character} is outside line {line}"))
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
