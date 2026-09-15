//! code_intel tool surface: JSON arg parsing, schema and compact rendering.
//! The heavy lifting lives in the CodeIntelligence implementation; this
//! module defines the stable agent contract: one tool, six read-only actions,
//! human 1-based positions, bounded results.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::execution::{CodeIntelContinuation, CodeIntelHeaderKind, CodeIntelPresentation};
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
        "description": "Use semantic Rust navigation for definitions (with a bounded preview), references, types/hover, document or workspace symbols and diagnostics. Use search for literal text, strings, configuration or documentation; text matches are not semantic references.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": CODE_INTEL_ACTIONS,
                    "description": "symbol, definition, references, hover, diagnostics, status"
                },
                "path": {"type": "string", "description": "file path relative to the workspace; for symbol, selects document outline"},
                "line": {"type": "integer", "minimum": 1, "description": "1-based line"},
                "column": {"type": "integer", "minimum": 1, "description": "1-based character within the line"},
                "symbol": {"type": "string", "description": "optional symbol name for headers"},
                "query": {"type": "string", "description": "symbol name/query for workspace search; with path, ranks matching document symbols first"},
                "include_info": {"type": "boolean", "description": "include info/hint diagnostics (default false)"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_CODE_INTEL_RESULTS},
                "offset": {"type": "integer", "minimum": 0, "description": "references/symbol paging: 0-based index of the first result to return"},
                "revision": {"type": "integer", "minimum": 0, "description": "continuation token from a previous page's \"revision\" field; rejected when the workspace or server changed since"}
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

fn offset(args: &Value) -> usize {
    args.get("offset")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}

fn prepared_offset(args: &Value) -> Result<usize, String> {
    match args.get("offset") {
        None => Ok(0),
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| "offset must be a non-negative integer".to_owned()),
        Some(_) => Err("offset must be a non-negative integer".to_owned()),
    }
}

fn revision(args: &Value) -> Option<u64> {
    args.get("revision").and_then(Value::as_u64)
}

fn prepared_revision(args: &Value) -> Result<Option<u64>, String> {
    match args.get("revision") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| "revision must be a non-negative integer".to_owned()),
    }
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
                offset: offset(args),
                revision: revision(args),
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
                offset: offset(args),
                revision: revision(args),
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
                offset: prepared_offset(args)?,
                revision: prepared_revision(args)?,
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
                offset: prepared_offset(args)?,
                revision: prepared_revision(args)?,
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

const SYMBOL_ANNOTATION_BUDGET_BYTES: usize = 2048;
const MAX_SYMBOL_ANNOTATION_BYTES: usize = 120;
const SYMBOL_ANNOTATION_TRUNCATION_NOTICE: &str =
    "\nsymbol details truncated (120 bytes per field; 2048 bytes total)";

fn symbol_annotation_order(symbols: &[Value], query: Option<&str>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..symbols.len()).collect();
    let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) else {
        return order;
    };
    let lower_query = query.to_lowercase();
    order.sort_by_key(|index| {
        (
            symbol_match_rank(&symbols[*index], query, &lower_query),
            *index,
        )
    });
    order
}

fn symbol_match_rank(symbol: &Value, query: &str, lower_query: &str) -> u8 {
    let name = symbol
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let lower_name = name.to_lowercase();
    if name == query {
        return 0;
    }
    if lower_name == lower_query {
        return 1;
    }
    if name.starts_with(query) || lower_name.starts_with(lower_query) {
        return 2;
    }
    if name.contains(query) || lower_name.contains(lower_query) {
        return 3;
    }
    let container = symbol
        .get("container")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let lower_container = container.to_lowercase();
    if container.contains(query) || lower_container.contains(lower_query) {
        return 4;
    }
    5
}

fn symbol_excerpt(value: &str, max_bytes: usize) -> (String, bool) {
    let mut excerpt = String::new();
    for character in value.chars() {
        let character = if character == '\r' || character == '\n' {
            ' '
        } else {
            character
        };
        if excerpt.len().saturating_add(character.len_utf8()) > max_bytes {
            return (excerpt, true);
        }
        excerpt.push(character);
    }
    (excerpt, false)
}

/// Appends one annotation without exceeding aggregate UTF-8 byte budget.
/// Returns whether source value was omitted or truncated.
fn append_symbol_annotation(
    out: &mut String,
    value: &str,
    label: &str,
    budget: &mut usize,
) -> bool {
    if value.is_empty() {
        return false;
    }
    let prefix = format!(" | {label}: ");
    let Some(available) = budget.checked_sub(prefix.len()) else {
        return true;
    };
    if available == 0 {
        return true;
    }
    let (mut excerpt, truncated) =
        symbol_excerpt(value, available.min(MAX_SYMBOL_ANNOTATION_BYTES));
    if truncated {
        let marker = "…";
        let value_budget = available.saturating_sub(marker.len());
        if value_budget == 0 {
            return true;
        }
        (excerpt, _) = symbol_excerpt(
            value,
            value_budget.min(MAX_SYMBOL_ANNOTATION_BYTES.saturating_sub(marker.len())),
        );
    }
    if excerpt.is_empty() {
        return true;
    }
    out.push_str(&prefix);
    out.push_str(&excerpt);
    if truncated {
        out.push('…');
    }
    *budget = budget
        .saturating_sub(prefix.len() + excerpt.len() + usize::from(truncated) * '…'.len_utf8());
    truncated
}

fn symbol_annotations(
    symbols: &[Value],
    query: Option<&str>,
    budget: usize,
) -> (Vec<String>, bool, usize) {
    let annotation_order = symbol_annotation_order(symbols, query);
    let mut annotations = vec![String::new(); symbols.len()];
    let mut annotation_budget = budget;
    let mut annotations_truncated = false;
    for index in annotation_order {
        let symbol = &symbols[index];
        let mut annotation = String::new();
        let mut symbol_truncated = false;
        // Detail/signature is more useful than a repeated container. Nested
        // document symbols encode hierarchy in name indentation already.
        for (field, label) in [("detail", "detail"), ("container", "in")] {
            let Some(value) = symbol.get(field).and_then(Value::as_str) else {
                continue;
            };
            symbol_truncated |=
                append_symbol_annotation(&mut annotation, value, label, &mut annotation_budget);
        }
        annotations_truncated |= symbol_truncated;
        annotations[index] = annotation;
    }
    (annotations, annotations_truncated, annotation_budget)
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

/// Captures the semantic records that may be presented under an aggregate
/// request budget. The complete rendered response remains available in
/// `full`; record boundaries here are derived from the structured outcome, so
/// continuation metadata is never recovered by parsing model-facing text.
pub(crate) fn presentation_for_code_intel(
    action: &str,
    outcome: &CodeIntelOutcome,
    full: String,
) -> CodeIntelPresentation {
    let meta = meta_line(&outcome.meta);
    if outcome.payload.get("error").is_some() {
        return CodeIntelPresentation {
            full,
            header: format!("code_intel {action}: {}", outcome_error(outcome)),
            records: Vec::new(),
            header_kind: CodeIntelHeaderKind::Static,
            continuation: None,
            meta,
        };
    }
    match action {
        "references" => references_presentation(outcome, full, meta),
        "symbol" => symbols_presentation(outcome, full, meta),
        "diagnostics" => diagnostics_presentation(outcome, full, meta),
        "status" => status_presentation(outcome, full, meta),
        "definition" => definition_presentation(outcome, full, meta),
        "hover" => hover_presentation(outcome, full, meta),
        _ => CodeIntelPresentation {
            full: full.clone(),
            header: full,
            records: Vec::new(),
            header_kind: CodeIntelHeaderKind::Static,
            continuation: None,
            meta: String::new(),
        },
    }
}

fn status_presentation(
    outcome: &CodeIntelOutcome,
    full: String,
    meta: String,
) -> CodeIntelPresentation {
    let mut records = Vec::new();
    if let Some(summary) = outcome.payload.get("summary").and_then(Value::as_str) {
        if !summary.is_empty() {
            records.push(summary.to_owned());
        }
    }
    if let Some(servers) = outcome.payload.get("servers").and_then(Value::as_array) {
        for server in servers {
            let state = server.get("state").and_then(Value::as_str).unwrap_or("?");
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
            let mut row = format!("- {state} root={root} binary={binary}");
            if let Some(open) = server.get("open_documents").and_then(Value::as_u64) {
                row.push_str(&format!(" open_documents={open}"));
            }
            if let Some(indexing) = server.get("indexing").and_then(Value::as_bool) {
                row.push_str(&format!(" indexing={indexing}"));
            }
            if server
                .get("binary_missing")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                row.push_str(" binary_missing=true");
            }
            records.push(row);
        }
    }
    CodeIntelPresentation {
        full,
        header: "code_intel status".into(),
        records,
        header_kind: CodeIntelHeaderKind::Static,
        continuation: None,
        meta,
    }
}

fn definition_presentation(
    outcome: &CodeIntelOutcome,
    full: String,
    meta: String,
) -> CodeIntelPresentation {
    let found = outcome
        .payload
        .get("found")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut header = String::from("code_intel definition");
    let mut records = Vec::new();
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
        if let Some(symbol) = outcome
            .payload
            .get("symbol")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            header = format!("code_intel definition: {symbol}");
        }
        records.push(format!("{file}:{line}:{column}"));
        if let Some(preview) = outcome.payload.get("preview").and_then(Value::as_str) {
            records.push(format!("  {preview}"));
        }
    } else {
        header = format!("code_intel definition: {}", {
            let received = outcome
                .payload
                .get("locations_received")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let skipped = outcome
                .payload
                .get("locations_out_of_scope")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if received > 0 && skipped == received {
                format!("{received} location(s) reported, all outside the workspace scope")
            } else if skipped > 0 {
                format!("not found in workspace ({skipped} out-of-scope location(s) skipped)")
            } else {
                "not found".into()
            }
        });
    }
    CodeIntelPresentation {
        full,
        header,
        records,
        header_kind: CodeIntelHeaderKind::Static,
        continuation: None,
        meta,
    }
}

fn hover_presentation(
    outcome: &CodeIntelOutcome,
    full: String,
    meta: String,
) -> CodeIntelPresentation {
    let found = outcome
        .payload
        .get("found")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let truncated = outcome
        .payload
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut header = String::from("code_intel hover");
    let mut records = Vec::new();
    if found {
        if truncated {
            header.push_str(" [truncated]");
        }
        records.push(
            outcome
                .payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        );
    } else {
        header.push_str(": no documentation");
    }
    CodeIntelPresentation {
        full,
        header,
        records,
        header_kind: CodeIntelHeaderKind::Static,
        continuation: None,
        meta,
    }
}

fn references_presentation(
    outcome: &CodeIntelOutcome,
    full: String,
    meta: String,
) -> CodeIntelPresentation {
    let total = outcome
        .payload
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let files = outcome
        .payload
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let file_count = outcome
        .payload
        .get("total_files")
        .and_then(Value::as_u64)
        .unwrap_or(files.len() as u64);
    let completeness = completeness_name(outcome.meta.completeness);
    let header = match outcome
        .payload
        .get("symbol")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        Some(symbol) => {
            format!("code_intel references: {symbol} - {total} across {file_count} file(s) | {completeness}")
        }
        None => {
            format!("code_intel references: {total} across {file_count} file(s) | {completeness}")
        }
    };
    let mut records = Vec::new();
    for file in files {
        let name = file.get("file").and_then(Value::as_str).unwrap_or("");
        if let Some(results) = file.get("results").and_then(Value::as_array) {
            for result in results {
                let line = result.get("line").and_then(Value::as_u64).unwrap_or(0);
                let column = result.get("column").and_then(Value::as_u64).unwrap_or(0);
                let mut record = name.to_owned();
                record.push_str(&format!("\n  {line}:{column}"));
                if let Some(context) = result.get("context").and_then(Value::as_str) {
                    record.push_str(&format!(": {context}"));
                }
                records.push(record);
            }
        }
    }
    let continuation = continuation_for_page(outcome);
    CodeIntelPresentation {
        full,
        header,
        records,
        header_kind: CodeIntelHeaderKind::References {
            symbol: outcome
                .payload
                .get("symbol")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            total: total as usize,
            file_count: file_count as usize,
            completeness: completeness.to_owned(),
        },
        continuation,
        meta,
    }
}

fn symbols_presentation(
    outcome: &CodeIntelOutcome,
    full: String,
    meta: String,
) -> CodeIntelPresentation {
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
    let query = outcome.payload.get("query").and_then(Value::as_str);
    let total = outcome.payload.get("total").and_then(Value::as_u64);
    let header = match (kind, total) {
        ("document", Some(total)) => format!(
            "code_intel action=symbol (document): {} of {} shown",
            symbols.len(),
            total
        ),
        ("document", None) => format!(
            "code_intel action=symbol (document): {} shown",
            symbols.len()
        ),
        (_, Some(total)) => format!(
            "code_intel action=symbol (workspace): {} - {} of {} shown",
            query.unwrap_or(""),
            symbols.len(),
            total
        ),
        _ => format!(
            "code_intel action=symbol (workspace): {} - {} shown",
            query.unwrap_or(""),
            symbols.len()
        ),
    };
    let (annotations, _, _) = symbol_annotations(&symbols, query, SYMBOL_ANNOTATION_BUDGET_BYTES);
    let records = symbols
        .iter()
        .enumerate()
        .map(|(index, symbol)| {
            let name = symbol.get("name").and_then(Value::as_str).unwrap_or("");
            let symbol_kind = symbol
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("symbol");
            let mut record = match (
                symbol.get("file").and_then(Value::as_str),
                symbol.get("line").and_then(Value::as_u64),
                symbol.get("column").and_then(Value::as_u64),
            ) {
                (Some(file), Some(line), Some(column)) => {
                    format!("{symbol_kind:>12} {name}  -> {file}:{line}:{column}")
                }
                (Some(file), _, _) => {
                    format!("{symbol_kind:>12} {name}  -> {file} (position unavailable)")
                }
                _ => format!("{symbol_kind:>12} {name}"),
            };
            record.push_str(&annotations[index]);
            record
        })
        .collect();
    CodeIntelPresentation {
        full,
        header,
        records,
        header_kind: CodeIntelHeaderKind::Symbols {
            document: kind == "document",
            query: query.map(str::to_owned),
            total: total.map(|value| value as usize),
        },
        continuation: continuation_for_page(outcome),
        meta,
    }
}

fn diagnostics_presentation(
    outcome: &CodeIntelOutcome,
    full: String,
    meta: String,
) -> CodeIntelPresentation {
    let files = outcome
        .payload
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let shown: u64 = files
        .iter()
        .map(|file| file.get("count").and_then(Value::as_u64).unwrap_or(0))
        .sum();
    let counts = match outcome.payload.get("total").and_then(Value::as_u64) {
        Some(total) => format!("{shown} shown of {total}"),
        None => format!("{shown} shown; total unknown"),
    };
    let header = if outcome.payload.get("scope").and_then(Value::as_str) == Some("published") {
        format!("code_intel diagnostics (published cache): {counts}")
    } else {
        format!("code_intel diagnostics: {counts}")
    };
    let mut records = Vec::new();
    for file in &files {
        let file_name = file.get("file").and_then(Value::as_str).unwrap_or("");
        if let Some(diagnostics) = file.get("diagnostics").and_then(Value::as_array) {
            for diagnostic in diagnostics {
                let severity = diagnostic
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("diagnostic");
                let line = diagnostic.get("line").and_then(Value::as_u64).unwrap_or(0);
                let column = diagnostic
                    .get("column")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let message = diagnostic
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                records.push(format!("{severity} {file_name}:{line}:{column}: {message}"));
            }
        } else if file.get("received").and_then(Value::as_bool) == Some(false) {
            records.push(format!("{file_name}: awaiting diagnostics publication"));
        }
    }
    let mut continuation = continuation_for_page(outcome);
    if outcome
        .payload
        .get("storage_truncated")
        .and_then(Value::as_bool)
        == Some(true)
    {
        if let Some(value) = continuation.as_mut() {
            value.scan_notice = Some("diagnostic cache truncated".into());
        } else {
            continuation = Some(CodeIntelContinuation {
                offset: None,
                next_offset: None,
                revision: None,
                scan_notice: Some("diagnostic cache truncated".into()),
            });
        }
    }
    CodeIntelPresentation {
        full,
        header,
        records,
        header_kind: CodeIntelHeaderKind::Diagnostics {
            published: outcome.payload.get("scope").and_then(Value::as_str) == Some("published"),
            total: outcome
                .payload
                .get("total")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
        },
        continuation,
        meta,
    }
}

fn continuation_for_page(outcome: &CodeIntelOutcome) -> Option<CodeIntelContinuation> {
    let has_more = outcome.payload.get("has_more").and_then(Value::as_bool) == Some(true);
    let next = outcome
        .payload
        .get("next_offset")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok());
    let offset = outcome
        .payload
        .get("offset")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok());
    let revision = outcome.payload.get("revision").and_then(Value::as_u64);
    let scan_notice = outcome
        .payload
        .get("scan_truncated")
        .and_then(Value::as_bool)
        .filter(|value| *value)
        .map(|_| {
            let received = outcome
                .payload
                .get("received")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let scanned = outcome
                .payload
                .get("scanned")
                .and_then(Value::as_u64)
                .unwrap_or(received);
            format!("server reported {received} locations; only the first {scanned} were scanned")
        });
    if !has_more
        && next.is_none()
        && offset.is_none()
        && revision.is_none()
        && scan_notice.is_none()
    {
        return None;
    }
    Some(CodeIntelContinuation {
        offset,
        next_offset: next,
        revision,
        scan_notice,
    })
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
            if let Some(preview) = outcome.payload.get("preview").and_then(Value::as_str) {
                out.push_str(&format!("\n  {preview}"));
            }
            let in_scope = outcome
                .payload
                .get("locations_received")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_sub(
                    outcome
                        .payload
                        .get("locations_out_of_scope")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
            if in_scope > 1 {
                out.push_str(&format!(
                    "\n  ({in_scope} in-scope locations; showing the first)"
                ));
            }
        } else {
            let received = outcome
                .payload
                .get("locations_received")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let skipped = outcome
                .payload
                .get("locations_out_of_scope")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if received > 0 && skipped == received {
                out.push_str(&format!(
                    "code_intel definition: {received} location(s) reported, all outside the workspace scope"
                ));
            } else if skipped > 0 {
                out.push_str(&format!(
                    "code_intel definition: not found in workspace ({skipped} out-of-scope location(s) skipped)"
                ));
            } else {
                out.push_str("code_intel definition: not found");
            }
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
    let next_offset = outcome.payload.get("next_offset").and_then(Value::as_u64);
    let revision = outcome.payload.get("revision").and_then(Value::as_u64);
    let total_files = outcome.payload.get("total_files").and_then(Value::as_u64);
    let scan_truncated = outcome
        .payload
        .get("scan_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let received = outcome
        .payload
        .get("received")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let scanned = outcome
        .payload
        .get("scanned")
        .and_then(Value::as_u64)
        .unwrap_or(received);
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
    let file_count = total_files.unwrap_or(files.len() as u64);
    match symbol {
        Some(symbol) => out.push_str(&format!(
            "code_intel references: {symbol} - {total} across {file_count} file(s) | {completeness}"
        )),
        None => out.push_str(&format!(
            "code_intel references: {total} across {file_count} file(s) | {completeness}"
        )),
    }
    if has_more {
        match (next_offset, revision) {
            // Executable continuation: offset pages are bounded and do not
            // depend on the model guessing a larger max_results; `revision`
            // binds the page to the workspace state it was enumerated under.
            (Some(next), Some(revision)) => out.push_str(&format!(
                " (showing {shown}; pass \"offset\": {next}, \"revision\": {revision} for the next page)"
            )),
            (Some(next), None) => out.push_str(&format!(
                " (showing {shown}; pass \"offset\": {next} for the next page)"
            )),
            (None, _) => out.push_str(&format!(" (showing {shown})")),
        }
    }
    if scan_truncated {
        out.push_str(&format!(
            "\n[server reported {received} locations; only the first {scanned} were scanned]"
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
        out.push_str(&format!(
            "code_intel action=symbol: {}",
            outcome_error(outcome)
        ));
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
    let query = outcome.payload.get("query").and_then(Value::as_str);
    let total = outcome.payload.get("total").and_then(Value::as_u64);
    match (kind, total) {
        ("document", Some(total)) => out.push_str(&format!(
            "code_intel action=symbol (document): {} of {} shown",
            symbols.len(),
            total
        )),
        ("document", None) => out.push_str(&format!(
            "code_intel action=symbol (document): {} shown",
            symbols.len()
        )),
        (_, Some(total)) => out.push_str(&format!(
            "code_intel action=symbol (workspace): {} - {} of {} shown",
            query.unwrap_or(""),
            symbols.len(),
            total
        )),
        _ => {
            out.push_str(&format!(
                "code_intel action=symbol (workspace): {} - {} shown",
                query.unwrap_or(""),
                symbols.len()
            ));
        }
    }

    // Basic rows stay in server/flattened order. Only annotation ownership is
    // ranked, so a strong textual match cannot lose its detail to earlier
    // irrelevant rows. Annotation bytes include labels and UTF-8 safely.
    let (mut annotations, mut annotations_truncated, remaining_budget) =
        symbol_annotations(&symbols, query, SYMBOL_ANNOTATION_BUDGET_BYTES);
    // Count truncation notice inside same aggregate budget. Reserve it only
    // when needed, so complete small responses keep all available bytes.
    if annotations_truncated && remaining_budget < SYMBOL_ANNOTATION_TRUNCATION_NOTICE.len() {
        (annotations, annotations_truncated, _) = symbol_annotations(
            &symbols,
            query,
            SYMBOL_ANNOTATION_BUDGET_BYTES - SYMBOL_ANNOTATION_TRUNCATION_NOTICE.len(),
        );
    }

    for (index, symbol) in symbols.iter().enumerate() {
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
            (Some(file), _, _) => out.push_str(&format!(
                "\n{symbol_kind:>12} {name}  -> {file} (position unavailable)"
            )),
            _ => out.push_str(&format!("\n{symbol_kind:>12} {name}")),
        }
        out.push_str(&annotations[index]);
    }
    if annotations_truncated {
        out.push_str(SYMBOL_ANNOTATION_TRUNCATION_NOTICE);
    }
    if outcome.payload.get("has_more").and_then(Value::as_bool) == Some(true) {
        // Executable continuation: `revision` binds the page to the workspace
        // state it was enumerated under; a changed workspace is rejected
        // instead of mixing pages.
        let next_offset = outcome.payload.get("next_offset").and_then(Value::as_u64);
        let revision = outcome.payload.get("revision").and_then(Value::as_u64);
        match (next_offset, revision) {
            (Some(next), Some(revision)) => out.push_str(&format!(
                "\nmore results; pass \"offset\": {next}, \"revision\": {revision} for the next page"
            )),
            (Some(next), None) => out.push_str(&format!(
                "\nmore results; pass \"offset\": {next} for the next page"
            )),
            _ => out.push_str("\nmore results omitted"),
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
    let shown: u64 = files
        .iter()
        .map(|file| file.get("count").and_then(Value::as_u64).unwrap_or(0))
        .sum();
    let counts = match outcome.payload.get("total").and_then(Value::as_u64) {
        Some(total) => format!("{shown} shown of {total}"),
        None => format!("{shown} shown; total unknown"),
    };
    if outcome.payload.get("scope").and_then(Value::as_str) == Some("published") {
        out.push_str(&format!(
            "code_intel diagnostics (published cache): {counts}"
        ));
    } else {
        out.push_str(&format!("code_intel diagnostics: {counts}"));
    }
    if outcome.payload.get("has_more").and_then(Value::as_bool) == Some(true) {
        out.push_str("\nmore results omitted");
    }
    if outcome
        .payload
        .get("storage_truncated")
        .and_then(Value::as_bool)
        == Some(true)
    {
        out.push_str("\ndiagnostic cache truncated");
    }
    for file in files {
        let file_name = file.get("file").and_then(Value::as_str).unwrap_or("");
        if file.get("received").and_then(Value::as_bool) == Some(false) {
            out.push_str(&format!("\n{file_name}: awaiting diagnostics publication"));
        }
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
    fn definition_preview_reaches_agent_renderer() {
        let mut outcome = CodeIntelOutcome::unavailable("fixture", "unused");
        outcome.payload =
            json!({"found":true,"file":"src/a.rs","line":1,"column":1,"preview":"fn target() {}"});
        let rendered = render_code_intel("definition", &outcome);
        assert!(rendered.contains("src/a.rs:1:1\n  fn target() {}"));
    }

    #[test]
    fn symbol_annotations_preserve_semantics_locations_and_reliability() {
        let mut outcome = CodeIntelOutcome::unavailable("fixture", "unused");
        outcome.meta.document_version = Some(7);
        outcome.meta.stale = true;
        outcome.payload = json!({"kind":"document", "symbols":[
            {"name":"Parser", "kind":"struct", "file":"src/a.rs", "line":1,"column":12},
            {"name":"  parse", "kind":"function", "file":"src/a.rs", "line":3,"column":12,
             "detail":"fn(value: &str) -> usize"},
            {"name":"parse", "kind":"function", "file":"src/b.rs", "line":null,"column":null,
             "container":"mod_é🚀", "detail":"fn(e\u{301}: &str)\r\n-> bool"}
        ]});
        let output = render_code_intel("symbol", &outcome);
        assert!(output.contains("  parse  -> src/a.rs:3:12 | detail: fn(value: &str) -> usize"));
        assert!(output
            .contains("src/b.rs (position unavailable) | detail: fn(e\u{301}: &str)  -> bool"));
        assert!(output.contains("| in: mod_é🚀"));
        assert!(!output.contains('\r'));
        assert!(output.contains("document_version: 7 | stale: true"));
    }

    #[test]
    fn symbol_annotations_have_per_field_and_aggregate_limits() {
        let mut outcome = CodeIntelOutcome::unavailable("fixture", "unused");
        outcome.payload = json!({"kind":"document", "has_more":true, "symbols":
            (0..100).map(|index| json!({"name":format!("f{index}"), "kind":"function",
                "file":"src/a.rs", "line":index+1, "column":1,
                "detail":"🚀".repeat(500), "container":"é".repeat(500)
            })).collect::<Vec<_>>()});
        let output = render_code_intel("symbol", &outcome);
        let annotation_value_bytes = output.matches('🚀').count() * '🚀'.len_utf8()
            + output.matches('é').count() * 'é'.len_utf8();
        assert!(annotation_value_bytes > 0);
        assert!(annotation_value_bytes <= SYMBOL_ANNOTATION_BUDGET_BYTES);
        assert!(!output.contains(&"🚀".repeat(31)));
        assert!(!output.contains(&"é".repeat(61)));
        assert!(
            output.contains("src/a.rs:100:1"),
            "budget must not hide locations"
        );
        let mut without_annotations = outcome;
        for symbol in without_annotations.payload["symbols"]
            .as_array_mut()
            .unwrap()
        {
            symbol.as_object_mut().unwrap().remove("detail");
            symbol.as_object_mut().unwrap().remove("container");
        }
        let basic_output = render_code_intel("symbol", &without_annotations);
        assert!(
            output.len().saturating_sub(basic_output.len()) <= SYMBOL_ANNOTATION_BUDGET_BYTES,
            "annotation bytes exceed budget"
        );
        assert!(output.contains("symbol details truncated"));
        assert!(output.contains("more results omitted"));
    }

    #[test]
    fn symbol_annotations_prioritize_strong_matches_without_reordering() {
        let mut symbols = (0..94)
            .map(|index| {
                json!({
                    "name": format!("other_{index}"),
                    "kind": "function",
                    "file": "src/lib.rs",
                    "line": index + 1,
                    "column": 1,
                    "detail": "x".repeat(500),
                    "container": "noise",
                })
            })
            .collect::<Vec<_>>();
        symbols.extend([
            json!({"name":"parse","kind":"function","file":"src/lib.rs","line":95,"column":1,"detail":"fn(value: &str) -> usize","container":"ModuleA"}),
            json!({"name":"parse","kind":"function","file":"src/lib.rs","line":96,"column":1,"detail":"fn(other: &str) -> bool","container":"ModuleB"}),
            json!({"name":"PARSE","kind":"function","file":"src/lib.rs","line":97,"column":1,"detail":"fn(case: &str) -> u8","container":"ModuleC"}),
            json!({"name":"parse_extra","kind":"function","file":"src/lib.rs","line":98,"column":1,"detail":"fn(extra: &str) -> u8","container":"ModuleD"}),
            json!({"name":"wrapper_parse","kind":"function","file":"src/lib.rs","line":99,"column":1,"detail":"fn(wrapped: &str) -> u8","container":"ModuleE"}),
            json!({"name":"method","kind":"function","file":"src/lib.rs","line":100,"column":1,"detail":"fn(method: &str) -> u8","container":"Parser"}),
        ]);
        let mut outcome = CodeIntelOutcome::unavailable("fixture", "unused");
        outcome.payload = json!({
            "kind": "workspace",
            "query": "parse",
            "symbols": symbols,
        });
        let output = render_code_intel("symbol", &outcome);
        for (name, detail, container) in [
            ("parse", "fn(value: &str) -> usize", "ModuleA"),
            ("parse", "fn(other: &str) -> bool", "ModuleB"),
            ("PARSE", "fn(case: &str) -> u8", "ModuleC"),
            ("parse_extra", "fn(extra: &str) -> u8", "ModuleD"),
            ("wrapper_parse", "fn(wrapped: &str) -> u8", "ModuleE"),
            ("method", "fn(method: &str) -> u8", "Parser"),
        ] {
            let line = output
                .lines()
                .find(|line| {
                    line.contains(&format!(" {name}  ->"))
                        && line.contains(&format!("in: {container}"))
                })
                .expect("candidate row");
            assert!(line.contains(&format!("detail: {detail}")), "{line}");
        }
        let first = output
            .lines()
            .position(|line| line.contains("other_0  ->"))
            .expect("first server row");
        let exact = output
            .lines()
            .position(|line| line.contains("parse  ->"))
            .expect("exact server row");
        assert!(first < exact, "annotation ranking must not reorder rows");
    }

    #[test]
    fn adding_irrelevant_symbols_does_not_consume_exact_match_annotation() {
        fn render(count: usize) -> String {
            let mut symbols = (0..count.saturating_sub(1))
                .map(|index| {
                    json!({
                        "name": format!("noise_{index}"),
                        "kind": "function",
                        "file": "src/lib.rs",
                        "line": index + 1,
                        "column": 1,
                        "detail": "x".repeat(500),
                    })
                })
                .collect::<Vec<_>>();
            symbols.push(json!({
                "name": "wanted",
                "kind": "function",
                "file": "src/lib.rs",
                "line": count,
                "column": 1,
                "detail": "fn(value: &str) -> usize",
            }));
            let mut outcome = CodeIntelOutcome::unavailable("fixture", "unused");
            outcome.payload = json!({
                "kind": "workspace",
                "query": "wanted",
                "symbols": symbols,
            });
            render_code_intel("symbol", &outcome)
        }

        for count in [3, 20, 100] {
            let output = render(count);
            let line = output
                .lines()
                .find(|line| line.contains("wanted  ->"))
                .expect("wanted row");
            assert!(
                line.contains("detail: fn(value: &str) -> usize"),
                "count={count}: {line}"
            );
        }
    }

    #[test]
    #[ignore = "manual release sequence measurement; no network or model"]
    fn measure_symbol_detail_sequence() {
        use crate::provider::{
            OpenAiCodexAdapter, ProviderAdapter, ProviderConfig, ProviderMessage, ProviderToolCall,
        };
        use std::time::{Instant, SystemTime, UNIX_EPOCH};

        let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
            "http://127.0.0.1:1",
            "fixture-model",
            "fixture-token",
            "fixture-account",
        ))
        .unwrap();
        let tools =
            super::super::ToolRegistry::default().definitions_for_mode(crate::OperatingMode::Auto);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "slim-symbol-sequence-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("lib.rs");
        for count in [5, 20, 100] {
            let source = (0..count)
                .map(|i| format!("pub fn parse_{i}(value: &str) -> usize {{ value.len() }}\n"))
                .collect::<String>();
            std::fs::write(&path, &source).unwrap();
            let mut after = CodeIntelOutcome::unavailable("fixture", "unused");
            after.meta.state = CodeIntelServerState::Ready;
            after.meta.document_version = Some(1);
            after.payload = json!({"kind":"document", "query":format!("parse_{}", count - 1), "symbols":(0..count).map(|i|json!({
                "name":format!("parse_{i}"), "kind":"function", "file":"lib.rs",
                "line":i+1, "column":8, "detail":"fn(value: &str) -> usize"
            })).collect::<Vec<_>>()});
            // Before this change the renderer ignored precisely these two
            // fields. Removing them reproduces its output, not a second renderer.
            // The semantic response is the shared prefix of both sequences;
            // this timer covers rendering, the native read (when needed), and
            // separately the next provider request preparation, without a model.
            let mut before = after.clone();
            for symbol in before.payload["symbols"].as_array_mut().unwrap() {
                symbol.as_object_mut().unwrap().remove("detail");
                symbol.as_object_mut().unwrap().remove("container");
            }
            let target = format!(" parse_{}  ->", count - 1);
            let mut samples = [Vec::new(), Vec::new()];
            for repetition in 0..11 {
                for mode in if repetition % 2 == 0 { [0, 1] } else { [1, 0] } {
                    let outcome = if mode == 0 { &before } else { &after };
                    let registry = super::super::ToolRegistry::default();
                    let start = Instant::now();
                    let rendered = render_code_intel("symbol", outcome);
                    let row = rendered
                        .lines()
                        .find(|line| line.contains(&target))
                        .unwrap();
                    let sufficient = row.contains("value: &str") && row.contains("-> usize");
                    let read = if sufficient {
                        None
                    } else {
                        let prepared = registry.prepare_invocation(
                            crate::OperatingMode::Auto,
                            &root,
                            "read",
                            &format!(r#"{{"path":"lib.rs","offset":{count},"max_lines":1}}"#),
                        );
                        let read = registry.execute_prepared_with_cancellation_and_progress(
                            &prepared,
                            None,
                            |_| {},
                        );
                        assert!(read.result.success, "{}", read.result.output);
                        Some(read)
                    };
                    let local_ns = start.elapsed().as_nanos();
                    let basic_rendered = if mode == 0 {
                        rendered.clone()
                    } else {
                        render_code_intel("symbol", &before)
                    };
                    let fully_enriched = rendered
                        .lines()
                        .filter(|line| line.contains("detail: fn(value: &str) -> usize"))
                        .count();
                    let annotation_bytes = rendered.len().saturating_sub(basic_rendered.len());
                    let evidence = read
                        .as_ref()
                        .map_or(row, |read| read.result.output.as_str());
                    assert!(evidence.contains("value: &str") && evidence.contains("-> usize"));
                    let mut messages = vec![
                        ProviderMessage::user(format!(
                            "What are the argument and return types of parse_{}?",
                            count - 1
                        )),
                        ProviderMessage::assistant(
                            "",
                            vec![ProviderToolCall {
                                id: "symbols".into(),
                                name: "code_intel".into(),
                                arguments: format!(
                                    r#"{{"action":"symbol","path":"lib.rs","query":"parse_{}","max_results":{}}}"#,
                                    count - 1,
                                    count
                                ),
                            }],
                        ),
                        ProviderMessage::tool("code_intel", "symbols", &rendered),
                    ];
                    if let Some(page) = &read {
                        messages.push(ProviderMessage::assistant(
                            "",
                            vec![ProviderToolCall {
                                id: "read".into(),
                                name: "read".into(),
                                arguments: format!(
                                    r#"{{"path":"lib.rs","offset":{count},"max_lines":1}}"#
                                ),
                            }],
                        ));
                        messages.push(ProviderMessage::tool("read", "read", &page.result.output));
                    }
                    let start = Instant::now();
                    let prepared = adapter
                        .prepare_messages_request_with_tools_checked(&messages, &tools)
                        .unwrap();
                    let preparation_ns = start.elapsed().as_nanos();
                    let result_bytes =
                        rendered.len() + read.as_ref().map_or(0, |page| page.result.output.len());
                    samples[mode].push((
                        local_ns,
                        preparation_ns,
                        rendered.len(),
                        result_bytes,
                        prepared.body.len(),
                        usize::from(read.is_some()),
                        read.as_ref().map_or(0, |page| page.receipt.bytes_read),
                        fully_enriched,
                        annotation_bytes,
                    ));
                }
            }
            for (mode, rows) in samples.iter().enumerate() {
                let stats = |index: usize| {
                    let mut values = rows
                        .iter()
                        .map(|row| if index == 0 { row.0 } else { row.1 })
                        .collect::<Vec<_>>();
                    values.sort_unstable();
                    format!("{}[{}..{}]", values[5], values[0], values[10])
                };
                let row = rows[0];
                println!("symbol_sequence count={count} mode={mode} n=11 required_invocations={} shared_lsp_response=1 source_opens={} read_bytes={} symbol_result_bytes={} all_result_bytes={} next_request_bytes={} fully_enriched={} target_result_position={} annotation_bytes={} render_plus_native_read_ns={} next_prepare_ns={} final=argument_str_return_usize",
                    1+row.5,row.5,row.6,row.2,row.3,row.4,row.7,count,row.8,stats(0),stats(1));
            }
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&root).unwrap();
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
        assert!(
            !text.contains("C:/Users/demo"),
            "absolute segments leak: {text}"
        );
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

    #[test]
    fn renders_diagnostics_publication_status() {
        let mut outcome = CodeIntelOutcome {
            meta: CodeIntelMeta {
                server: "rust-analyzer".into(),
                state: CodeIntelServerState::Ready,
                completeness: CodeIntelCompleteness::Unknown,
                document_version: Some(1),
                stale: false,
                elapsed_ms: 0,
            },
            payload: json!({ "files": [{ "file": "src/main.rs", "count": 0, "received": false, "diagnostics": [] }] }),
        };
        let text = render_code_intel("diagnostics", &outcome);
        assert!(text.contains("src/main.rs: awaiting diagnostics publication"));
        assert!(text.contains("completeness: unknown"));
        outcome.payload = json!({ "scope": "published", "files": [] });
        assert!(render_code_intel("diagnostics", &outcome).contains("published cache"));
    }
}
