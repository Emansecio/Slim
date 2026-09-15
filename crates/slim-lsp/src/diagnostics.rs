//! DiagnosticsStore: the client-side copy of server-published diagnostics
//! (textDocument/publishDiagnostics) with the version they were published for.
//! Diagnostics are keyed by URI and bounded per URI and in total, so a chatty
//! server can never flood the model with an unbounded blob.

use std::collections::{HashMap, VecDeque};

use lsp_types::{Diagnostic, DiagnosticSeverity};
use url::Url;

pub const DEFAULT_MAX_DIAGNOSTICS_PER_URI: usize = 200;
pub const DEFAULT_MAX_DIAGNOSTIC_URIS: usize = 256;
/// A single diagnostic message is capped so a pathological publication
/// (macro-expanded errors can carry megabytes of text) cannot pin memory.
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 8 * 1024;
/// Byte budget for the retained items of one URI, counted after the severity
/// selection and count cap. Dropping continues from the tail, which the sort
/// leaves at the lowest severities.
pub const DEFAULT_MAX_DIAGNOSTIC_BYTES_PER_URI: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct StoredDiagnostics {
    pub uri: Url,
    /// The document version this batch was published for, when the server sent one.
    pub version: Option<i64>,
    /// A document sync happened after this publication (including reopen).
    pub stale: bool,
    pub total: usize,
    pub total_without_info: usize,
    pub truncated: bool,
    pub items: Vec<Diagnostic>,
}

#[derive(Debug, Default)]
pub struct DiagnosticsStore {
    by_uri: HashMap<Url, StoredDiagnostics>,
    insertion_order: VecDeque<Url>,
    max_per_uri: usize,
    max_uris: usize,
    max_bytes_per_uri: usize,
    evicted: bool,
    revision: u64,
}

impl DiagnosticsStore {
    pub fn new(max_per_uri: usize, max_uris: usize) -> Self {
        Self::with_limits(max_per_uri, max_uris, DEFAULT_MAX_DIAGNOSTIC_BYTES_PER_URI)
    }

    pub fn with_limits(max_per_uri: usize, max_uris: usize, max_bytes_per_uri: usize) -> Self {
        Self {
            by_uri: HashMap::new(),
            insertion_order: VecDeque::new(),
            max_per_uri: max_per_uri.max(1),
            max_uris: max_uris.max(1),
            max_bytes_per_uri: max_bytes_per_uri.max(1),
            evicted: false,
            revision: 0,
        }
    }

    /// Stores one publishDiagnostics payload; newest wins, capped per URI.
    /// URIs are evicted oldest-inserted-first (FIFO), so a recent file is
    /// never dropped while a stale one survives.
    pub fn set(&mut self, uri: Url, version: Option<i64>, mut items: Vec<Diagnostic>) {
        self.revision = self.revision.wrapping_add(1);
        let total = items.len();
        let total_without_info = items.iter().filter(|item| included(item, false)).count();
        // Severity first: a late error must not be evicted by early hints.
        // Position order is preserved within the same severity.
        items.sort_by_key(|item| {
            (
                severity_rank(item.severity),
                item.range.start.line,
                item.range.start.character,
            )
        });
        items.truncate(self.max_per_uri);
        // Byte budget on the retained items: after the severity sort the tail
        // is the lowest-value end, so truncating there drops hints first.
        let mut used = 0usize;
        let mut kept = items.len();
        for (index, item) in items.iter_mut().enumerate() {
            cap_message(&mut item.message);
            used = used.saturating_add(estimated_bytes(item));
            if used > self.max_bytes_per_uri {
                kept = index;
                break;
            }
        }
        let byte_limited = kept < items.len();
        items.truncate(kept);
        let truncated = total > self.max_per_uri || byte_limited;
        if !self.by_uri.contains_key(&uri) {
            while self.by_uri.len() >= self.max_uris {
                let Some(oldest) = self.insertion_order.pop_front() else {
                    break;
                };
                if self.by_uri.remove(&oldest).is_some() {
                    self.evicted = true;
                    break;
                }
            }
            self.insertion_order.push_back(uri.clone());
        }
        self.by_uri.insert(
            uri.clone(),
            StoredDiagnostics {
                uri,
                version,
                stale: false,
                total,
                total_without_info,
                truncated,
                items,
            },
        );
    }

    pub fn get(&self, uri: &Url) -> Option<&StoredDiagnostics> {
        self.by_uri.get(uri)
    }

    pub fn invalidate(&mut self, uri: &Url) {
        if let Some(stored) = self.by_uri.get_mut(uri) {
            stored.stale = true;
        }
    }

    /// Counts the latest retained publications, before per-URI item caps.
    /// After URI eviction the overall total is unknown until a new store.
    pub fn totals(&self, include_info: bool) -> (Option<usize>, bool) {
        let total = self
            .by_uri
            .values()
            .map(|stored| {
                if include_info {
                    stored.total
                } else {
                    stored.total_without_info
                }
            })
            .sum();
        let truncated = self.evicted || self.by_uri.values().any(|stored| stored.truncated);
        ((!self.evicted).then_some(total), truncated)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn len(&self) -> usize {
        self.by_uri.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_uri.is_empty()
    }

    pub fn uris(&self) -> impl Iterator<Item = &Url> {
        self.by_uri.keys()
    }

    /// Diagnostics filtered for agent consumption: errors and warnings by
    /// default, infos/hints only when requested.
    pub fn agent_view(&self, uri: &Url, include_info: bool) -> Vec<&Diagnostic> {
        self.get(uri)
            .map(|stored| {
                stored
                    .items
                    .iter()
                    .filter(|item| included(item, include_info))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Storage order: errors before warnings before info/hints. An omitted
/// severity is reported as an error, per the LSP spec.
fn severity_rank(severity: Option<DiagnosticSeverity>) -> u8 {
    match severity {
        None | Some(DiagnosticSeverity::ERROR) => 0,
        Some(DiagnosticSeverity::WARNING) => 1,
        Some(DiagnosticSeverity::INFORMATION) => 2,
        Some(DiagnosticSeverity::HINT) => 3,
        Some(_) => 4,
    }
}

fn included(item: &Diagnostic, include_info: bool) -> bool {
    include_info
        || !matches!(
            item.severity,
            Some(DiagnosticSeverity::INFORMATION | DiagnosticSeverity::HINT)
        )
}

/// Caps one message at a char boundary; the marker keeps the cut visible to
/// the agent instead of silently ending the text.
fn cap_message(message: &mut String) {
    if message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES {
        return;
    }
    let mut end = MAX_DIAGNOSTIC_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str("\n…[truncated]");
}

/// Rough stored size of one diagnostic: the message dominates, related
/// information adds its own messages, the constant covers range/code/source.
fn estimated_bytes(item: &Diagnostic) -> usize {
    let related = item
        .related_information
        .as_ref()
        .map(|items| {
            items
                .iter()
                .map(|related| related.message.len())
                .sum::<usize>()
        })
        .unwrap_or(0);
    128usize
        .saturating_add(item.message.len())
        .saturating_add(related)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn diag(line: u32, severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 1 },
            },
            severity: Some(severity),
            code: None,
            code_description: None,
            source: Some("mock".into()),
            message: message.into(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    fn url(path: &str) -> Url {
        Url::from_file_path(path).expect("file url")
    }

    #[test]
    fn stores_and_filters_by_severity() {
        let mut store = DiagnosticsStore::new(10, 4);
        let uri = url("D:/demo/main.rs");
        store.set(
            uri.clone(),
            Some(3),
            vec![
                diag(1, DiagnosticSeverity::ERROR, "type mismatch"),
                diag(2, DiagnosticSeverity::WARNING, "unused"),
                diag(4, DiagnosticSeverity::INFORMATION, "hint"),
                diag(5, DiagnosticSeverity::HINT, "style"),
            ],
        );
        let view = store.agent_view(&uri, false);
        assert_eq!(view.len(), 2);
        assert_eq!(view[0].message, "type mismatch");
        assert_eq!(view[1].message, "unused");

        let with_info = store.agent_view(&uri, true);
        assert_eq!(with_info.len(), 4);
    }

    #[test]
    fn caps_per_uri_and_keeps_latest() {
        let mut store = DiagnosticsStore::new(2, 4);
        let uri = url("D:/demo/main.rs");
        let items: Vec<_> = (0..5)
            .map(|i| diag(i, DiagnosticSeverity::ERROR, &format!("e{i}")))
            .collect();
        store.set(uri.clone(), Some(1), items);
        let stored = store.get(&uri).expect("stored");
        assert_eq!(stored.items.len(), 2);
        assert_eq!(stored.items[0].range.start.line, 0);
        assert_eq!(store.totals(false), (Some(5), true));
        store.set(uri.clone(), Some(2), vec![]);
        assert_eq!(store.totals(false), (Some(0), false));
    }

    #[test]
    fn evicts_oldest_inserted_uri_first() {
        let mut store = DiagnosticsStore::new(10, 2);
        let a = url("D:/demo/a.rs");
        let b = url("D:/demo/b.rs");
        let c = url("D:/demo/c.rs");
        let d = url("D:/demo/d.rs");
        store.set(
            a.clone(),
            Some(1),
            vec![diag(0, DiagnosticSeverity::ERROR, "a")],
        );
        store.set(
            b.clone(),
            Some(1),
            vec![diag(0, DiagnosticSeverity::ERROR, "b")],
        );
        store.set(
            c.clone(),
            Some(1),
            vec![diag(0, DiagnosticSeverity::ERROR, "c")],
        );
        assert!(store.get(&a).is_none());
        assert!(store.get(&b).is_some());
        assert!(store.get(&c).is_some());
        // Overwriting b does not refresh its insertion age: d evicts b, not c.
        store.set(
            b.clone(),
            Some(2),
            vec![diag(0, DiagnosticSeverity::ERROR, "b2")],
        );
        store.set(
            d.clone(),
            Some(1),
            vec![diag(0, DiagnosticSeverity::ERROR, "d")],
        );
        assert!(store.get(&b).is_none());
        assert!(store.get(&c).is_some());
        assert!(store.get(&d).is_some());
        assert_eq!(store.totals(false), (None, true));
    }

    #[test]
    fn error_after_many_hints_survives_cap() {
        let mut store = DiagnosticsStore::new(200, 4);
        let uri = url("D:/demo/main.rs");
        let mut items: Vec<_> = (0..250)
            .map(|i| diag(i, DiagnosticSeverity::HINT, "style"))
            .collect();
        items.push(diag(999, DiagnosticSeverity::ERROR, "late error"));
        store.set(uri.clone(), Some(1), items);
        let view = store.agent_view(&uri, false);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].message, "late error");
        // Position order still holds inside one severity.
        let with_info = store.agent_view(&uri, true);
        assert_eq!(with_info.len(), 200);
        assert_eq!(with_info[0].message, "late error");
    }

    #[test]
    fn empty_publish_clears_previous() {
        let mut store = DiagnosticsStore::new(10, 4);
        let uri = url("D:/demo/main.rs");
        store.set(
            uri.clone(),
            Some(1),
            vec![diag(0, DiagnosticSeverity::ERROR, "boom")],
        );
        assert_eq!(store.agent_view(&uri, false).len(), 1);
        store.set(uri.clone(), Some(2), vec![]);
        assert_eq!(store.agent_view(&uri, false).len(), 0);
    }

    #[test]
    fn byte_budget_drops_the_lowest_severity_tail() {
        // ~230 bytes estimated per item (100-byte message + overhead), so a
        // 400-byte budget fits only one: the error survives and both hints
        // drop even though the count cap (200) was never reached.
        let mut store = DiagnosticsStore::with_limits(200, 4, 400);
        let uri = url("D:/demo/main.rs");
        let hundred = "x".repeat(100);
        store.set(
            uri.clone(),
            Some(1),
            vec![
                diag(10, DiagnosticSeverity::HINT, &hundred),
                diag(9, DiagnosticSeverity::ERROR, &hundred),
                diag(11, DiagnosticSeverity::HINT, &hundred),
            ],
        );
        let stored = store.get(&uri).expect("stored");
        assert_eq!(stored.total, 3, "totals still describe the publication");
        assert!(stored.truncated, "byte eviction must mark truncated");
        assert_eq!(stored.items.len(), 1);
        assert_eq!(stored.items[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(store.totals(true), (Some(3), true));
        assert_eq!(store.totals(false), (Some(1), true));
    }

    #[test]
    fn oversized_message_is_capped_at_a_char_boundary() {
        let mut store = DiagnosticsStore::new(200, 4);
        let uri = url("D:/demo/main.rs");
        // One ASCII byte plus two-byte chars puts the 8 KiB cut inside a
        // multibyte char, so the boundary must round down, never split it.
        let message = format!("a{}", "á".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES));
        store.set(
            uri.clone(),
            Some(1),
            vec![diag(0, DiagnosticSeverity::ERROR, &message)],
        );
        let stored = store.get(&uri).expect("stored").items[0].message.clone();
        assert!(stored.ends_with("[truncated]"));
        assert!(stored.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES + 16);
    }
}
