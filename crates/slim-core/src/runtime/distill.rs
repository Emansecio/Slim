//! Universal deterministic output distiller (0 ms, 0 LLM tokens).
//!
//! 1. **JSON-to-Table** (`distill_json_for_model`): compresses homogeneous JSON
//!    arrays (root arrays or common wrapper envelopes like `{ items/results/data/... }`)
//!    into compact single-header tables with constant-column hoisting and empty
//!    column pruning, typically cutting MCP payload tokens by 55%–80%.
//! 2. **Generic Log Distiller** (`distill_generic_log`): strips residual ANSI/CR
//!    noise, collapses non-consecutive repetitive lines via drain3-light
//!    structural templates, and preserves error/warning lines + head + tail.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use serde_json::{Map, Value};

const MIN_JSON_DISTILL_BYTES: usize = 1_024;
const MAX_JSON_DISTILL_BYTES: usize = 4 * 1024 * 1024;
const MIN_TABLE_ITEMS: usize = 4;
const MAX_TABLE_COLS: usize = 20;
const MAX_CELL_CHARS: usize = 240;
const MIN_LOG_DISTILL_BYTES: usize = 1_536;

/// Result of distilling a JSON payload into a compact model-facing string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DistilledJson {
    pub(crate) text: String,
    pub(crate) kind: &'static str,
    pub(crate) original_bytes: usize,
    pub(crate) distilled_bytes: usize,
}

/// Attempts deterministic compression of a JSON payload for LLM consumption.
/// Returns `None` when input is not JSON, outside size bounds, or saves < 30%.
pub(crate) fn distill_json_for_model(raw: &str) -> Option<DistilledJson> {
    let orig = raw.len();
    if !(MIN_JSON_DISTILL_BYTES..=MAX_JSON_DISTILL_BYTES).contains(&orig) {
        return None;
    }
    let trimmed = raw.trim();
    if !(trimmed.starts_with('[') || trimmed.starts_with('{')) {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;

    let (candidate, kind) = match &value {
        Value::Array(items) => {
            if let Some(table) = try_render_table(None, items, None) {
                (table, "JSON->table")
            } else {
                (compact_pruned_json(&value), "JSON->compact")
            }
        }
        Value::Object(map) => {
            if let Some(table) = try_render_envelope_table(map) {
                (table, "JSON->table")
            } else {
                (compact_pruned_json(&value), "JSON->compact")
            }
        }
        _ => return None,
    };

    let distilled_bytes = candidate.len();
    // Require at least 30% byte reduction to justify rewriting the shape.
    if distilled_bytes > orig.saturating_mul(70) / 100 {
        return None;
    }
    Some(DistilledJson {
        text: candidate,
        kind,
        original_bytes: orig,
        distilled_bytes,
    })
}

fn try_render_envelope_table(map: &Map<String, Value>) -> Option<String> {
    // Find the largest array-of-objects field (common MCP envelopes: items,
    // results, data, rows, issues, nodes, entries, records, files, etc.).
    let mut best_key: Option<&str> = None;
    let mut best_slice: Option<&[Value]> = None;
    for (k, v) in map {
        if let Value::Array(arr) = v {
            if arr.len() >= MIN_TABLE_ITEMS
                && arr.iter().all(Value::is_object)
                && best_slice.is_none_or(|b| arr.len() > b.len())
            {
                best_key = Some(k.as_str());
                best_slice = Some(arr.as_slice());
            }
        }
    }
    let (array_key, items) = (best_key?, best_slice?);

    // Collect compact scalar metadata from sibling envelope keys.
    let mut meta_parts = Vec::new();
    for (k, v) in map {
        if k == array_key || is_empty_value(v) {
            continue;
        }
        let summary = format_cell(v);
        if !summary.is_empty() {
            meta_parts.push(format!("{k}={summary}"));
        }
    }
    let meta = (!meta_parts.is_empty()).then(|| meta_parts.join(", "));
    try_render_table(Some(array_key), items, meta.as_deref())
}

fn try_render_table(
    array_key: Option<&str>,
    items: &[Value],
    envelope_meta: Option<&str>,
) -> Option<String> {
    if items.len() < MIN_TABLE_ITEMS || !items.iter().all(Value::is_object) {
        return None;
    }
    // Count key presence and check homogeneity (>= 60% average coverage across top keys).
    let mut key_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut key_order: Vec<&str> = Vec::new();
    for item in items {
        let obj = item.as_object()?;
        for (k, v) in obj {
            if is_empty_value(v) {
                continue;
            }
            let count = key_counts.entry(k.as_str()).or_insert(0);
            if *count == 0 {
                key_order.push(k.as_str());
            }
            *count += 1;
        }
    }
    if key_order.is_empty() || key_order.len() > MAX_TABLE_COLS * 2 {
        return None;
    }

    // Keep columns that appear in at least 10% of rows (capped at MAX_TABLE_COLS).
    let min_presence = (items.len() / 10).max(1);
    let active_cols: Vec<&str> = key_order
        .into_iter()
        .filter(|k| key_counts.get(k).copied().unwrap_or(0) >= min_presence)
        .take(MAX_TABLE_COLS)
        .collect();
    if active_cols.is_empty() {
        return None;
    }

    let total_cells = active_cols.len() * items.len();
    let filled_cells: usize = active_cols
        .iter()
        .map(|k| key_counts.get(k).copied().unwrap_or(0))
        .sum();
    if filled_cells.saturating_mul(100) < total_cells.saturating_mul(55) {
        return None;
    }

    // Hoist columns that have the exact same non-empty value on every row.
    let mut const_cols: Vec<(&str, String)> = Vec::new();
    let mut var_cols: Vec<&str> = Vec::new();
    for &col in &active_cols {
        if key_counts.get(col).copied().unwrap_or(0) == items.len() {
            let first = items[0].as_object().and_then(|m| m.get(col)).map(format_cell);
            if let Some(first_val) = first {
                let all_same = items[1..].iter().all(|it| {
                    it.as_object()
                        .and_then(|m| m.get(col))
                        .map(format_cell)
                        .as_deref()
                        == Some(first_val.as_str())
                });
                if all_same {
                    const_cols.push((col, first_val));
                    continue;
                }
            }
        }
        var_cols.push(col);
    }

    let mut out = String::with_capacity(items.len() * 64);
    if let Some(meta) = envelope_meta {
        let _ = writeln!(out, "meta: {meta}");
    }
    if let Some(key) = array_key {
        let _ = writeln!(out, "rows({key}): {}", items.len());
    } else {
        let _ = writeln!(out, "rows: {}", items.len());
    }
    if !const_cols.is_empty() {
        out.push_str("const: ");
        for (i, (k, v)) in const_cols.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{k}={v}");
        }
        out.push('\n');
    }
    if var_cols.is_empty() {
        return Some(out.trim_end().to_owned());
    }
    let _ = writeln!(out, "cols: {}", var_cols.join(" | "));
    for item in items {
        let Some(obj) = item.as_object() else { continue };
        for (i, &col) in var_cols.iter().enumerate() {
            if i > 0 {
                out.push_str(" | ");
            }
            if let Some(v) = obj.get(col) {
                if !is_empty_value(v) {
                    out.push_str(&format_cell(v));
                }
            }
        }
        out.push('\n');
    }
    Some(out.trim_end().to_owned())
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn format_cell(v: &Value) -> String {
    let raw = match v {
        Value::Null => return String::new(),
        Value::Bool(b) => return if *b { "true" } else { "false" }.to_owned(),
        Value::Number(n) => return n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => prune_json_value(v).to_string(),
    };
    let mut clean = String::with_capacity(raw.len().min(MAX_CELL_CHARS + 4));
    let mut chars = 0usize;
    for ch in raw.chars() {
        if chars >= MAX_CELL_CHARS {
            clean.push('…');
            break;
        }
        match ch {
            '\n' | '\r' | '\t' => clean.push(' '),
            '|' => clean.push('/'),
            c if c.is_control() => {}
            c => clean.push(c),
        }
        chars += 1;
    }
    clean.trim().to_owned()
}

fn prune_json_value(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut pruned = Map::new();
            for (k, val) in map {
                let p = prune_json_value(val);
                if !is_empty_value(&p) {
                    pruned.insert(k.clone(), p);
                }
            }
            Value::Object(pruned)
        }
        Value::Array(items) => {
            let pruned: Vec<Value> = items
                .iter()
                .map(prune_json_value)
                .filter(|val| !is_empty_value(val))
                .collect();
            Value::Array(pruned)
        }
        other => other.clone(),
    }
}

fn compact_pruned_json(v: &Value) -> String {
    prune_json_value(v).to_string()
}

/// Strips ANSI/CSI sequences and resolves single-line `\r` progress overwrites.
pub(crate) fn strip_ansi_and_cr(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut line = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if crate::mcp::skip_terminal_sequence(ch, &mut chars).is_some() {
            continue;
        }
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                let _ = chars.next();
                out.push_str(&line);
                out.push('\n');
                line.clear();
            } else {
                line.clear();
            }
            continue;
        }
        if ch == '\n' {
            out.push_str(&line);
            out.push('\n');
            line.clear();
            continue;
        }
        if !ch.is_control() || ch == '\t' {
            line.push(ch);
        }
    }
    if !line.is_empty() {
        out.push_str(&line);
    }
    out
}

/// Collapses repetitive non-consecutive log lines using a drain3-light
/// structural template while preserving errors, warnings, head, and tail.
pub(crate) fn distill_generic_log(raw: &str) -> Option<String> {
    if raw.len() < MIN_LOG_DISTILL_BYTES {
        return None;
    }
    let cleaned = strip_ansi_and_cr(raw);
    let lines: Vec<&str> = cleaned.lines().collect();
    if lines.len() < 40 {
        return None;
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    let templates: Vec<String> = lines
        .iter()
        .map(|line| {
            let tmpl = structural_template(line);
            *counts.entry(tmpl.clone()).or_insert(0) += 1;
            tmpl
        })
        .collect();

    let total = lines.len();
    let head_keep = 20usize;
    let tail_start = total.saturating_sub(30);
    let mut emitted_templates: HashMap<String, usize> = HashMap::new();
    let mut out = String::with_capacity(cleaned.len() / 2);
    let mut collapsed_lines = 0usize;

    for (idx, (&line, tmpl)) in lines.iter().zip(templates.iter()).enumerate() {
        let is_important = is_signal_line(line);
        let in_head_or_tail = idx < head_keep || idx >= tail_start;
        let freq = counts.get(tmpl).copied().unwrap_or(1);

        if is_important || (in_head_or_tail && freq < 8) {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if freq >= 4 {
            let seen = emitted_templates.entry(tmpl.clone()).or_insert(0);
            *seen += 1;
            if *seen == 1 {
                let _ = writeln!(out, "{line}");
                let _ = writeln!(out, "[×{} similar lines: {}]", freq - 1, truncate_chars(tmpl, 120));
            } else {
                collapsed_lines += 1;
            }
            continue;
        }

        if in_head_or_tail {
            out.push_str(line);
            out.push('\n');
        } else {
            collapsed_lines += 1;
        }
    }

    if collapsed_lines == 0 {
        return None;
    }
    if out.len() > raw.len().saturating_mul(70) / 100 {
        return None;
    }
    Some(out.trim_end().to_owned())
}

fn is_signal_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error")
        || lower.contains("failed")
        || lower.contains("failure")
        || lower.contains("panic")
        || lower.contains("exception")
        || lower.contains("traceback")
        || lower.contains("warning:")
        || lower.contains("fatal")
}

/// Replaces numbers, hex sequences, durations and timestamps with placeholders
/// so repetitive structured log lines hash to the same template.
fn structural_template(line: &str) -> String {
    let trimmed = line.trim();
    let mut out = String::with_capacity(trimmed.len().min(160));
    for tok in trimmed.split_whitespace().take(16) {
        if !out.is_empty() {
            out.push(' ');
        }
        if is_timestamp_token(tok) {
            out.push_str("<TS>");
            continue;
        }
        if is_hex_or_uuid_token(tok) {
            out.push_str("<HEX>");
            continue;
        }
        let mut in_num = false;
        for ch in tok.chars() {
            if ch.is_ascii_digit() {
                if !in_num {
                    out.push_str("<N>");
                    in_num = true;
                }
            } else {
                in_num = false;
                out.push(ch);
            }
        }
    }
    out
}

fn is_timestamp_token(tok: &str) -> bool {
    let t = tok.trim_matches(|c| matches!(c, '[' | ']' | '(' | ')'));
    t.len() >= 8
        && t.as_bytes().iter().filter(|&&b| b == b':').count() >= 2
        && t.as_bytes().first().is_some_and(u8::is_ascii_digit)
}

fn is_hex_or_uuid_token(tok: &str) -> bool {
    let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return hex.len() >= 4 && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    t.len() >= 12
        && t.bytes().any(|b| b.is_ascii_digit())
        && t.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_array_distills_into_compact_table_with_constant_hoisting() {
        let items: Vec<Value> = (1..=40)
            .map(|i| {
                json!({
                    "id": i,
                    "repo": "slim-desktop",
                    "status": "open",
                    "title": format!("Issue number {i} with details"),
                    "assignee": Value::Null,
                    "labels": [],
                    "body_empty": "",
                })
            })
            .collect();
        let raw = serde_json::to_string_pretty(&json!({
            "total": 40,
            "items": items
        }))
        .unwrap();

        let distilled = distill_json_for_model(&raw).expect("should distill homogeneous JSON");
        assert_eq!(distilled.kind, "JSON->table");
        assert!(
            distilled.distilled_bytes * 100 < distilled.original_bytes * 45,
            "expected >=55% reduction, got {} -> {}",
            distilled.original_bytes,
            distilled.distilled_bytes
        );
        assert!(distilled.text.contains("const: repo=slim-desktop, status=open"));
        assert!(distilled.text.contains("cols: id | title"));
        assert!(!distilled.text.contains("assignee"));
        assert!(distilled.text.contains("40 | Issue number 40 with details"));
    }

    #[test]
    fn generic_log_distiller_collapses_repetitive_lines_and_keeps_errors() {
        let mut log = String::new();
        for i in 0..80 {
            let _ = writeln!(
                log,
                "\x1b[32m[12:00:{:02}] Connecting to worker {} took {}ms\x1b[0m",
                i % 60,
                i,
                10 + i
            );
            if i == 42 {
                log.push_str("ERROR: database connection refused on port 5432\n");
            }
        }
        let distilled = distill_generic_log(&log).expect("should collapse repetitive log");
        assert!(distilled.contains("ERROR: database connection refused on port 5432"));
        assert!(distilled.contains("similar lines:"));
        assert!(!distilled.contains("\x1b["));
        assert!(distilled.len() * 100 < log.len() * 50);
    }
}
