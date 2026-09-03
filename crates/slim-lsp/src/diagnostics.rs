//! DiagnosticsStore: the client-side copy of server-published diagnostics
//! (textDocument/publishDiagnostics) with the version they were published for.
//! Diagnostics are keyed by URI and bounded per URI and in total, so a chatty
//! server can never flood the model with an unbounded blob.

use std::collections::{HashMap, VecDeque};

use lsp_types::{Diagnostic, DiagnosticSeverity};
use url::Url;

pub const DEFAULT_MAX_DIAGNOSTICS_PER_URI: usize = 200;
pub const DEFAULT_MAX_DIAGNOSTIC_URIS: usize = 256;

#[derive(Clone, Debug)]
pub struct StoredDiagnostics {
    pub uri: Url,
    /// The document version this batch was published for, when the server sent one.
    pub version: Option<i64>,
    pub items: Vec<Diagnostic>,
}

#[derive(Debug, Default)]
pub struct DiagnosticsStore {
    by_uri: HashMap<Url, StoredDiagnostics>,
    insertion_order: VecDeque<Url>,
    max_per_uri: usize,
    max_uris: usize,
}

impl DiagnosticsStore {
    pub fn new(max_per_uri: usize, max_uris: usize) -> Self {
        Self {
            by_uri: HashMap::new(),
            insertion_order: VecDeque::new(),
            max_per_uri: max_per_uri.max(1),
            max_uris: max_uris.max(1),
        }
    }

    /// Stores one publishDiagnostics payload; newest wins, capped per URI.
    /// URIs are evicted oldest-inserted-first (FIFO), so a recent file is
    /// never dropped while a stale one survives.
    pub fn set(&mut self, uri: Url, version: Option<i64>, mut items: Vec<Diagnostic>) {
        items.sort_by_key(|item| (item.range.start.line, item.range.start.character));
        items.truncate(self.max_per_uri);
        if items.is_empty() && !self.by_uri.contains_key(&uri) {
            return;
        }
        if !self.by_uri.contains_key(&uri) {
            while self.by_uri.len() >= self.max_uris {
                let Some(oldest) = self.insertion_order.pop_front() else {
                    break;
                };
                if self.by_uri.remove(&oldest).is_some() {
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
                items,
            },
        );
    }

    pub fn get(&self, uri: &Url) -> Option<&StoredDiagnostics> {
        self.by_uri.get(uri)
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
                    .filter(|item| match item.severity {
                        Some(DiagnosticSeverity::ERROR) | Some(DiagnosticSeverity::WARNING) => true,
                        Some(DiagnosticSeverity::INFORMATION) | Some(DiagnosticSeverity::HINT) => {
                            include_info
                        }
                        _ => true,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
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
    }

    #[test]
    fn evicts_oldest_inserted_uri_first() {
        let mut store = DiagnosticsStore::new(10, 2);
        let a = url("D:/demo/a.rs");
        let b = url("D:/demo/b.rs");
        let c = url("D:/demo/c.rs");
        let d = url("D:/demo/d.rs");
        store.set(a.clone(), Some(1), vec![diag(0, DiagnosticSeverity::ERROR, "a")]);
        store.set(b.clone(), Some(1), vec![diag(0, DiagnosticSeverity::ERROR, "b")]);
        store.set(c.clone(), Some(1), vec![diag(0, DiagnosticSeverity::ERROR, "c")]);
        assert!(store.get(&a).is_none());
        assert!(store.get(&b).is_some());
        assert!(store.get(&c).is_some());
        // Overwriting b does not refresh its insertion age: d evicts b, not c.
        store.set(b.clone(), Some(2), vec![diag(0, DiagnosticSeverity::ERROR, "b2")]);
        store.set(d.clone(), Some(1), vec![diag(0, DiagnosticSeverity::ERROR, "d")]);
        assert!(store.get(&b).is_none());
        assert!(store.get(&c).is_some());
        assert!(store.get(&d).is_some());
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
}
