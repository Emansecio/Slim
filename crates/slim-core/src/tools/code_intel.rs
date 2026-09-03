//! code_intel tool surface: JSON arg parsing, schema and compact rendering.
//! The heavy lifting lives in the CodeIntelligence implementation; this
//! module defines the stable agent contract: one tool, six read-only actions,
//! human 1-based positions, bounded results.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::codeintel::{
    CodeIntelCompleteness, CodeIntelDiagnosticsQuery, CodeIntelMeta, CodeIntelOutcome,
    CodeIntelPositionQuery, CodeIntelServerState, CodeIntelSymbolQuery, DEFAULT_CODE_INTEL_LIMIT,
    MAX_CODE_INTEL_RESULTS,
};

#[derive(Clone, Debug)]
pub enum CodeIntelRequest {
    Status { workspace: PathBuf },
    Definition(CodeIntelPositionQuery),
    References(CodeIntelPositionQuery),
    Hover(CodeIntelPositionQuery),
    Symbols(CodeIntelSymbolQuery),
    Diagnostics(CodeIntelDiagnosticsQuery),
}

pub const CODE_INTEL_ACTIONS: &[&str] = &[
    "symbol",
    "definition",
    "references",
    "hover",
    "diagnostics",
    "status",
];

pub fn code_intel_definition() -> Value {
    json!({
        "name": "code_intel",
        "description": "Locates definitions, references, hover docs, symbol outlines and server diagnostics for Rust.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": CODE_INTEL_ACTIONS,
                    "description": "symbol, definition, references, hover, diagnostics, status"
                },
                "path": {"type": "string", "description": "file path relative to the workspace"},
                "line": {"type": "integer", "minimum": 1, "description": "1-based line"},
                "column": {"type": "integer", "minimum": 1, "description": "1-based character within the line"},
                "symbol": {"type": "string", "description": "optional symbol name for headers"},
                "query": {"type": "string", "description": "symbol query for action=symbol without path"},
                "include_info": {"type": "boolean", "description": "include info/hint diagnostics (default false)"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_CODE_INTEL_RESULTS}
            },
            "required": ["action"],
            "additionalProperties": false
        }
    })
}

fn required_str(args: &Value, name: &str) -> Result<String, String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing string argument: {name}"))
}

fn optional_u32(args: &Value, name: &str) -> Result<u32, String> {
    match args.get(name) {
        None => Ok(0),
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value >= 1)
            .ok_or_else(|| format!("{name} must be a positive integer")),
        Some(_) => Err(format!("{name} must be an integer")),
    }
}

fn max_results(args: &Value) -> usize {
    args.get("max_results")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_CODE_INTEL_LIMIT)
        .clamp(1, MAX_CODE_INTEL_RESULTS)
}

fn prepared_max_results(args: &Value) -> Result<usize, String> {
    let value = args
        .get("max_results")
        .and_then(Value::as_u64)
        .ok_or_else(|| "max_results must be a positive integer".to_owned())?;
    Ok(usize::try_from(value)
        .unwrap_or(usize::MAX)
        .clamp(1, MAX_CODE_INTEL_RESULTS))
}

fn optional_prepared_string(args: &Value, name: &str) -> Result<Option<String>, String> {
    args.get(name)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{name} must be a string"))
        })
        .transpose()
}

fn safe_resolve_path(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    super::resolve_workspace_path(cwd, path)
}

/// Parses the tool arguments into a typed request. Paths are resolved
/// against cwd (the workspace root).
pub fn parse_code_intel_request(cwd: &Path, args: &Value) -> Result<CodeIntelRequest, String> {
    let action = required_str(args, "action")?;
    match action.as_str() {
        "status" => Ok(CodeIntelRequest::Status {
            workspace: cwd.to_path_buf(),
        }),
        "definition" | "references" | "hover" => {
            let path = safe_resolve_path(cwd, &required_str(args, "path")?)?;
            let line = optional_u32(args, "line")?;
            if line == 0 {
                return Err("line is required for this action".into());
            }
            let column = optional_u32(args, "column")?;
            if column == 0 {
                return Err("column is required for this action".into());
            }
            let query = CodeIntelPositionQuery {
                workspace: cwd.to_path_buf(),
                path,
                line,
                column,
                symbol: args
                    .get("symbol")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                max_results: max_results(args),
                cancellation: None,
            };
            match action.as_str() {
                "definition" => Ok(CodeIntelRequest::Definition(query)),
                "references" => Ok(CodeIntelRequest::References(query)),
                _ => Ok(CodeIntelRequest::Hover(query)),
            }
        }
        "symbol" => {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| safe_resolve_path(cwd, value))
                .transpose()?;
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|value| !value.trim().is_empty());
            if path.is_none() && query.is_none() {
                return Err(
                    "symbol needs a path (document outline) or a query (workspace search)".into(),
                );
            }
            Ok(CodeIntelRequest::Symbols(CodeIntelSymbolQuery {
                workspace: cwd.to_path_buf(),
                path,
                query,
                max_results: max_results(args),
                cancellation: None,
            }))
        }
        "diagnostics" => {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| safe_resolve_path(cwd, value))
                .transpose()?;
            let include_info = args
                .get("include_info")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(CodeIntelRequest::Diagnostics(CodeIntelDiagnosticsQuery {
                workspace: cwd.to_path_buf(),
                path,
                include_info,
                max_results: max_results(args),
                cancellation: None,
            }))
        }
        other => Err(format!("unknown action: {other}")),
    }
}

/// Builds the typed request from paths already resolved by tool preparation.
/// This performs no filesystem access and is therefore safe to consume later
/// from the async execution path.
pub(crate) fn parse_prepared_code_intel_request(
    workspace: &Path,
    args: &Value,
    resolved_path: Option<&Path>,
) -> Result<CodeIntelRequest, String> {
    let action = required_str(args, "action")?;
    let max_results = prepared_max_results(args)?;
    match action.as_str() {
        "status" => Ok(CodeIntelRequest::Status {
            workspace: workspace.to_path_buf(),
        }),
        "definition" | "references" | "hover" => {
            let path = resolved_path
                .ok_or_else(|| "missing string argument: path".to_owned())?
                .to_path_buf();
            let line = optional_u32(args, "line")?;
            if line == 0 {
                return Err("line is required for this action".into());
            }
            let column = optional_u32(args, "column")?;
            if column == 0 {
                return Err("column is required for this action".into());
            }
            let query = CodeIntelPositionQuery {
                workspace: workspace.to_path_buf(),
                path,
                line,
                column,
                symbol: optional_prepared_string(args, "symbol")?,
                max_results,
                cancellation: None,
            };
            match action.as_str() {
                "definition" => Ok(CodeIntelRequest::Definition(query)),
                "references" => Ok(CodeIntelRequest::References(query)),
                _ => Ok(CodeIntelRequest::Hover(query)),
            }
        }
        "symbol" => {
            let query =
                optional_prepared_string(args, "query")?.filter(|value| !value.trim().is_empty());
            if resolved_path.is_none() && query.is_none() {
                return Err(
                    "symbol needs a path (document outline) or a query (workspace search)".into(),
                );
            }
            Ok(CodeIntelRequest::Symbols(CodeIntelSymbolQuery {
                workspace: workspace.to_path_buf(),
                path: resolved_path.map(Path::to_path_buf),
                query,
                max_results,
                cancellation: None,
            }))
        }
        "diagnostics" => {
            let include_info = args
                .get("include_info")
                .and_then(Value::as_bool)
                .ok_or_else(|| "include_info must be a boolean".to_owned())?;
            Ok(CodeIntelRequest::Diagnostics(CodeIntelDiagnosticsQuery {
                workspace: workspace.to_path_buf(),
                path: resolved_path.map(Path::to_path_buf),
                include_info,
                max_results,
                cancellation: None,
            }))
        }
        other => Err(format!("unknown action: {other}")),
    }
}

/// Meta line shared by every result, so readers always know how fresh and
/// how complete the answer is.
fn meta_line(meta: &CodeIntelMeta) -> String {
    let mut line = format!(
        "server: {} | state: {} | completeness: {}",
        meta.server,
        state_name(meta.state),
        completeness_name(meta.completeness)
    );
    if let Some(version) = meta.document_version {
        line.push_str(&format!(" | document_version: {version}"));
    }
    if meta.stale {
        line.push_str(" | stale: true");
    }
    line.push_str(&format!(" | {}ms", meta.elapsed_ms));
    line
}

fn state_name(state: CodeIntelServerState) -> &'static str {
    match state {
        CodeIntelServerState::Unavailable => "unavailable",
        CodeIntelServerState::Starting => "starting",
        CodeIntelServerState::Indexing => "indexing",
        CodeIntelServerState::Ready => "ready",
        CodeIntelServerState::Degraded => "degraded",
        CodeIntelServerState::Stopped => "stopped",
    }
}

fn completeness_name(completeness: CodeIntelCompleteness) -> &'static str {
    match completeness {
        CodeIntelCompleteness::Complete => "complete",
        CodeIntelCompleteness::Partial => "partial",
        CodeIntelCompleteness::Unknown => "unknown",
    }
}

fn outcome_error(outcome: &CodeIntelOutcome) -> &str {
    outcome
        .payload
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
}

/// Leaf segment of a filesystem path for model-facing text. Absolute paths
/// stay in the structured payload; only the leaf is rendered.
fn path_leaf(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

/// Renders a CodeIntelOutcome into the compact text returned to the model.
pub fn render_code_intel(action: &str, outcome: &CodeIntelOutcome) -> String {
    match action {
        "status" => render_status(outcome),
        "definition" => render_definition(outcome),
        "references" => render_references(outcome),
        "hover" => render_hover(outcome),
        "symbol" => render_symbols(outcome),
        "diagnostics" => render_diagnostics(outcome),
        _ => format!("{action}\n{}", meta_line(&outcome.meta)),
    }
}

fn render_status(outcome: &CodeIntelOutcome) -> String {
    if let Some(error) = outcome.payload.get("error").and_then(Value::as_str) {
        return format!("code_intel status: {error}\n{}", meta_line(&outcome.meta));
    }
    let summary = outcome
        .payload
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let servers = outcome
        .payload
        .get("servers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = String::from("code_intel status");
    if !summary.is_empty() {
        out.push_str(&format!("\n{summary}"));
    }
    for server in servers {
        let state = server.get("state").and_then(Value::as_str).unwrap_or("?");
        // Absolute paths carry machine-local segments (e.g. the user name);
        // the model only needs the leaf to tell servers apart.
        let root = server
            .get("root")
            .and_then(Value::as_str)
            .map(path_leaf)
            .unwrap_or("");
        let binary = server
            .get("binary")
            .and_then(Value::as_str)
            .map(path_leaf)
            .unwrap_or("");
        let mut line = format!("\n- {state} root={root} binary={binary}");
        if let Some(open) = server.get("open_documents").and_then(Value::as_u64) {
            line.push_str(&format!(" open_documents={open}"));
        }
        if let Some(indexing) = server.get("indexing").and_then(Value::as_bool) {
            line.push_str(&format!(" indexing={indexing}"));
        }
        if server
            .get("binary_missing")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            line.push_str(" binary_missing=true");
        }
        out.push_str(&line);
    }
    out.push_str(&format!("\n{}", meta_line(&outcome.meta)));
    out
}

fn render_definition(outcome: &CodeIntelOutcome) -> String {
    let mut out = String::new();
    if outcome.payload.get("error").is_some() {
        out.push_str(&format!(
            "code_intel definition: {}",
            outcome_error(outcome)
        ));
    } else {
        let found = outcome
            .payload
            .get("found")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if found {
            let file = outcome
                .payload
                .get("file")
                .and_then(Value::as_str)
                .unwrap_or("");
            let line = outcome
                .payload
                .get("line")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let column = outcome
                .payload
                .get("column")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let symbol = outcome
                .payload
                .get("symbol")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            match symbol {
                Some(symbol) => {
                    out.push_str(&format!(
                        "code_intel definition: {symbol}\n{file}:{line}:{column}"
                    ));
                }
                None => out.push_str(&format!("code_intel definition\n{file}:{line}:{column}")),
            }
        } else {
            out.push_str("code_intel definition: not found");
        }
    }
    out.push_str(&format!("\n{}", meta_line(&outcome.meta)));
    out
}

fn render_references(outcome: &CodeIntelOutcome) -> String {
    let mut out = String::new();
    if outcome.payload.get("error").is_some() {
        out.push_str(&format!(
            "code_intel references: {}",
            outcome_error(outcome)
        ));
        out.push_str(&format!("\n{}", meta_line(&outcome.meta)));
        return out;
    }
    let total = outcome
        .payload
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let shown = outcome
        .payload
        .get("shown")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let has_more = outcome
        .payload
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let completeness = completeness_name(outcome.meta.completeness);
    let symbol = outcome
        .payload
        .get("symbol")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let files = outcome
        .payload
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let file_count = files.len();
    match symbol {
        Some(symbol) => out.push_str(&format!(
            "code_intel references: {symbol} - {total} across {file_count} file(s) | {completeness}"
        )),
        None => out.push_str(&format!(
            "code_intel references: {total} across {file_count} file(s) | {completeness}"
        )),
    }
    if has_more {
        out.push_str(&format!(
            " (showing {shown}; increase max_results to return more)"
        ));
    }
    for file in files {
        let file_name = file.get("file").and_then(Value::as_str).unwrap_or("");
        out.push_str(&format!("\n{file_name}"));
        let results = file
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for result in results {
            let line = result.get("line").and_then(Value::as_u64).unwrap_or(0);
            let column = result.get("column").and_then(Value::as_u64).unwrap_or(0);
            match result.get("context").and_then(Value::as_str) {
                Some(context) => out.push_str(&format!("\n  {line}:{column}: {context}")),
                None => out.push_str(&format!("\n  {line}:{column}")),
            }
        }
    }
    out.push_str(&format!("\n{}", meta_line(&outcome.meta)));
    out
}

fn render_hover(outcome: &CodeIntelOutcome) -> String {
    let mut out = String::new();
    if outcome.payload.get("error").is_some() {
        out.push_str(&format!("code_intel hover: {}", outcome_error(outcome)));
        out.push_str(&format!("\n{}", meta_line(&outcome.meta)));
        return out;
    }
    let found = outcome
        .payload
        .get("found")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = outcome
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let truncated = outcome
        .payload
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if found {
        out.push_str("code_intel hover");
        if truncated {
            out.push_str(" [truncated]");
        }
        out.push_str(&format!("\n{text}"));
    } else {
        out.push_str("code_intel hover: no documentation");
    }
    out.push_str(&format!("\n{}", meta_line(&outcome.meta)));
    out
}

fn render_symbols(outcome: &CodeIntelOutcome) -> String {
    let mut out = String::new();
    if outcome.payload.get("error").is_some() {
        out.push_str(&format!("code_intel symbols: {}", outcome_error(outcome)));
        out.push_str(&format!("\n{}", meta_line(&outcome.meta)));
        return out;
    }
    let kind = outcome
        .payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let symbols = outcome
        .payload
        .get("symbols")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match kind {
        "document" => out.push_str(&format!("code_intel symbols (document): {}", symbols.len())),
        _ => {
            let query = outcome
                .payload
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("");
            out.push_str(&format!(
                "code_intel symbols (workspace): {query} - {} shown",
                symbols.len()
            ));
        }
    }
    for symbol in symbols {
        let name = symbol.get("name").and_then(Value::as_str).unwrap_or("");
        let symbol_kind = symbol
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("symbol");
        let file = symbol.get("file").and_then(Value::as_str);
        let line = symbol.get("line").and_then(Value::as_u64);
        let column = symbol.get("column").and_then(Value::as_u64);
        match (file, line, column) {
            (Some(file), Some(line), Some(column)) => {
                out.push_str(&format!(
                    "\n{symbol_kind:>12} {name}  -> {file}:{line}:{column}"
                ));
            }
            _ => out.push_str(&format!("\n{symbol_kind:>12} {name}")),
        }
    }
    out.push_str(&format!("\n{}", meta_line(&outcome.meta)));
    out
}

fn render_diagnostics(outcome: &CodeIntelOutcome) -> String {
    let mut out = String::new();
    if outcome.payload.get("error").is_some() {
        out.push_str(&format!(
            "code_intel diagnostics: {}",
            outcome_error(outcome)
        ));
        out.push_str(&format!("\n{}", meta_line(&outcome.meta)));
        return out;
    }
    let files = outcome
        .payload
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total: u64 = files
        .iter()
        .map(|file| file.get("count").and_then(Value::as_u64).unwrap_or(0))
        .sum();
    out.push_str(&format!("code_intel diagnostics: {total}"));
    for file in files {
        let file_name = file.get("file").and_then(Value::as_str).unwrap_or("");
        let diagnostics = file
            .get("diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for diagnostic in diagnostics {
            let severity = diagnostic
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("diagnostic");
            let code = diagnostic.get("code").and_then(Value::as_str);
            let source = diagnostic.get("source").and_then(Value::as_str);
            let line = diagnostic.get("line").and_then(Value::as_u64).unwrap_or(0);
            let column = diagnostic
                .get("column")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let message = diagnostic
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut row = format!("\n{severity} {file_name}:{line}:{column}");
            if let Some(code) = code {
                row.push_str(&format!(" [{code}]"));
            }
            if let Some(source) = source {
                row.push_str(&format!(" ({source})"));
            }
            row.push_str(&format!(": {message}"));
            out.push_str(&row);
        }
    }
    out.push_str(&format!("\n{}", meta_line(&outcome.meta)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_definition_with_human_positions() {
        let dir = std::env::temp_dir().join(format!("slim-ci-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let value = json!({
            "action": "definition",
            "path": "src/main.rs",
            "line": 12,
            "column": 3,
            "symbol": "run"
        });
        let request = parse_code_intel_request(&dir, &value).expect("parse");
        match request {
            CodeIntelRequest::Definition(query) => {
                assert_eq!(
                    query.path,
                    std::fs::canonicalize(&dir)
                        .expect("canonical workspace")
                        .join("src/main.rs")
                );
                assert_eq!(query.line, 12);
                assert_eq!(query.column, 3);
                assert_eq!(query.symbol.as_deref(), Some("run"));
                assert_eq!(query.max_results, DEFAULT_CODE_INTEL_LIMIT);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_bad_positions_and_unknown_actions() {
        let value = json!({ "action": "definition", "path": "a.rs", "line": 0 });
        assert!(parse_code_intel_request(Path::new("D:\\test"), &value).is_err());

        let value = json!({ "action": "nope" });
        assert!(parse_code_intel_request(Path::new("D:\\test"), &value).is_err());

        let value = json!({ "action": "definition" });
        assert!(parse_code_intel_request(Path::new("D:\\test"), &value).is_err());
    }

    #[test]
    fn symbol_needs_path_or_query() {
        let dir = std::env::temp_dir().join(format!("slim-ci-symbol-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("workspace");
        let value = json!({ "action": "symbol" });
        assert!(parse_code_intel_request(&dir, &value).is_err());
        let value = json!({ "action": "symbol", "query": "   " });
        assert!(parse_code_intel_request(&dir, &value).is_err());
        let value = json!({ "action": "symbol", "query": "execute" });
        assert!(parse_code_intel_request(&dir, &value).is_ok());
        let value = json!({ "action": "symbol", "path": "lib.rs" });
        assert!(parse_code_intel_request(&dir, &value).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prepared_symbol_rejects_whitespace_only_query() {
        let dir = std::env::temp_dir().join(format!("slim-ci-prepared-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("workspace");
        let value = json!({ "action": "symbol", "query": "   ", "max_results": 20 });
        assert!(parse_prepared_code_intel_request(&dir, &value, None).is_err());
        let value = json!({ "action": "symbol", "query": " execute ", "max_results": 20 });
        assert!(parse_prepared_code_intel_request(&dir, &value, None).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_path_escape() {
        let dir = std::env::temp_dir().join(format!("slim-ci-escape-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let value = json!({
            "action": "definition",
            "path": "../../../etc/passwd",
            "line": 1,
            "column": 1,
        });
        let result = parse_code_intel_request(&dir, &value);
        assert!(result.is_err(), "should reject escape: {:?}", result);
        let msg = result.unwrap_err();
        assert!(
            msg.contains("escapes"),
            "error should mention escapes, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renders_references_compact_and_grouped() {
        let outcome = CodeIntelOutcome {
            meta: CodeIntelMeta {
                server: "rust-analyzer".into(),
                state: CodeIntelServerState::Ready,
                completeness: CodeIntelCompleteness::Complete,
                document_version: Some(7),
                stale: false,
                elapsed_ms: 12,
            },
            payload: json!({
                "total": 18,
                "shown": 2,
                "has_more": true,
                "files": [
                    {
                        "file": "runtime/mod.rs",
                        "count": 2,
                        "results": [
                            {"line": 599, "column": 3, "context": "client.stream_messages(...)"},
                            {"line": 978, "column": 5, "context": "build_messages_request(...)"}
                        ]
                    }
                ],
                "symbol": "execute_tool_call"
            }),
        };
        let text = render_code_intel("references", &outcome);
        assert!(text.contains("execute_tool_call - 18 across 1 file(s) | complete"));
        assert!(text.contains("runtime/mod.rs"));
        assert!(text.contains("599:3: client.stream_messages(...)"));
        assert!(text.contains("document_version: 7"));
    }

    #[test]
    fn renders_status_with_leaf_paths_only() {
        let outcome = CodeIntelOutcome {
            meta: CodeIntelMeta {
                server: "rust-analyzer".into(),
                state: CodeIntelServerState::Ready,
                completeness: CodeIntelCompleteness::Complete,
                document_version: None,
                stale: false,
                elapsed_ms: 3,
            },
            payload: json!({
                "servers": [{
                    "server": "rust-analyzer",
                    "root": "C:/Users/demo/proj",
                    "binary": "C:/Users/demo/bin/rust-analyzer.exe",
                    "binary_missing": false,
                    "state": "ready",
                    "open_documents": 2,
                    "indexing": false
                }],
                "summary": "rust-analyzer ready (2 open document(s))"
            }),
        };
        let text = render_code_intel("status", &outcome);
        assert!(!text.contains("C:/Users/demo"), "absolute segments leak: {text}");
        assert!(text.contains("root=proj"), "{text}");
        assert!(text.contains("binary=rust-analyzer.exe"), "{text}");
    }

    #[test]
    fn renders_unavailable_status() {
        let outcome = CodeIntelOutcome::unavailable("rust-analyzer", "binary missing");
        let text = render_code_intel("status", &outcome);
        assert!(text.contains("state: unavailable"));
        assert!(text.contains("binary missing"));
    }
}
