//! Zero-dependency incremental multi-language semantic code graph.
//!
//! Built inside [`crate::project_index::ProjectIndexDaemon`] during the same
//! pass that warms the trigram index (or incrementally from FSEvents / local
//! writes), and persisted atomically to `~/.slim/cache/graphs/<hex>.v1.bin`.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::trigram::{cache_subdir_path, is_binary_bytes, MAX_INDEXED_FILE_BYTES};
use crate::tools::outline::{extract_outline_lines, OutlineLang};

const GRAPH_MAGIC: &[u8; 8] = b"SLIMGRP1";
const MAX_CALLEES_PER_SYMBOL: usize = 48;
const MAX_GRAPH_NODES: usize = 12_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphSymbol {
    pub kind: String,
    pub name: String,
    pub qualified: String,
    pub start_line: usize,
    pub end_line: usize,
    pub exported: bool,
    pub is_test: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileGraphNode {
    pub rel_path: String,
    pub len: u64,
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    pub is_test_file: bool,
    pub symbols: Vec<GraphSymbol>,
    pub raw_imports: Vec<String>,
    pub calls_by_symbol: Vec<Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GraphEdgeHit {
    pub rel_path: String,
    pub symbol: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub exported: bool,
    pub is_test: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImpactReport {
    pub target: String,
    pub definitions: Vec<GraphEdgeHit>,
    pub importers: Vec<String>,
    pub callers: Vec<GraphEdgeHit>,
    pub tests_covering: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct GraphSnapshotDisk {
    nodes: Vec<FileGraphNode>,
}

#[derive(Debug, Default)]
pub struct SemanticGraph {
    nodes: HashMap<String, FileGraphNode>,
    symbol_index: HashMap<String, Vec<(String, usize)>>,
    symbol_lower_index: HashMap<String, Vec<(String, usize)>>,
    importers_index: HashMap<String, Vec<String>>,
    callers_index: HashMap<String, Vec<(String, usize)>>,
    ready: bool,
    dirty_since_save: usize,
    importers_dirty: bool,
}

impl SemanticGraph {
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn mark_ready(&mut self) {
        self.ready = true;
        if self.importers_dirty {
            self.rebuild_importers_index();
        }
    }

    pub fn file_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn symbol_count(&self) -> usize {
        self.nodes.values().map(|n| n.symbols.len()).sum()
    }

    pub fn dirty_count(&self) -> usize {
        self.dirty_since_save
    }

    pub fn is_fresh(&self, rel_path: &str, len: u64, mtime_secs: u64, mtime_nanos: u32) -> bool {
        self.nodes.get(rel_path).is_some_and(|n| {
            n.len == len && n.mtime_secs == mtime_secs && n.mtime_nanos == mtime_nanos
        })
    }

    pub fn upsert_bytes(
        &mut self,
        rel_path: &str,
        len: u64,
        mtime_secs: u64,
        mtime_nanos: u32,
        bytes: &[u8],
    ) {
        if bytes.len() > MAX_INDEXED_FILE_BYTES || is_binary_bytes(bytes) {
            self.remove_rel(rel_path);
            return;
        }
        if !self.nodes.contains_key(rel_path) && self.nodes.len() >= MAX_GRAPH_NODES {
            return;
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            self.remove_rel(rel_path);
            return;
        };
        let Some(node) = parse_file_node(rel_path, len, mtime_secs, mtime_nanos, text) else {
            self.remove_rel(rel_path);
            return;
        };
        self.remove_from_indexes(rel_path);
        self.add_to_indexes(&node);
        self.nodes.insert(rel_path.to_owned(), node);
        self.dirty_since_save = self.dirty_since_save.saturating_add(1);
        self.importers_dirty = true;
        if self.ready {
            self.rebuild_importers_index();
        }
    }

    pub fn remove_rel(&mut self, rel_path: &str) -> bool {
        if self.nodes.remove(rel_path).is_some() {
            self.remove_from_indexes(rel_path);
            self.dirty_since_save = self.dirty_since_save.saturating_add(1);
            self.importers_dirty = true;
            if self.ready {
                self.rebuild_importers_index();
            }
            true
        } else {
            false
        }
    }

    pub fn retain_live_rel(&mut self, live_rels: &HashSet<String>) -> bool {
        let stale: Vec<String> = self
            .nodes
            .keys()
            .filter(|rel| !live_rels.contains(*rel))
            .cloned()
            .collect();
        if stale.is_empty() {
            return false;
        }
        for rel in stale {
            self.nodes.remove(&rel);
            self.remove_from_indexes(&rel);
            self.dirty_since_save = self.dirty_since_save.saturating_add(1);
        }
        self.importers_dirty = true;
        true
    }

    pub fn definitions(&self, symbol: &str, limit: usize) -> Vec<GraphEdgeHit> {
        let query = symbol.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut hits = Vec::new();
        if let Some(entries) = self.symbol_index.get(query) {
            self.push_symbol_hits(entries, &mut hits);
        }
        if hits.is_empty() {
            let lower = query.to_ascii_lowercase();
            if let Some(entries) = self.symbol_lower_index.get(&lower) {
                self.push_symbol_hits(entries, &mut hits);
            }
        }
        hits.sort_by(|a, b| {
            b.exported
                .cmp(&a.exported)
                .then_with(|| a.is_test.cmp(&b.is_test))
                .then_with(|| a.rel_path.cmp(&b.rel_path))
                .then_with(|| a.start_line.cmp(&b.start_line))
        });
        hits.truncate(limit);
        hits
    }

    pub fn file_symbols(&self, rel_path: &str) -> Vec<GraphSymbol> {
        self.nodes
            .get(rel_path)
            .map(|n| n.symbols.clone())
            .unwrap_or_default()
    }

    pub fn imports_and_importers(&self, rel_path: &str) -> (Vec<String>, Vec<String>) {
        let imports = self
            .nodes
            .get(rel_path)
            .map(|n| n.raw_imports.clone())
            .unwrap_or_default();
        let importers = self
            .importers_index
            .get(rel_path)
            .cloned()
            .unwrap_or_default();
        (imports, importers)
    }

    pub fn callers(&self, symbol: &str, limit: usize) -> Vec<GraphEdgeHit> {
        let query = symbol.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let short = query.rsplit("::").next().unwrap_or(query);
        let short = short.rsplit('.').next().unwrap_or(short);

        // Identify defining files so we can prioritize callers that also import them.
        let mut defining_importers: HashSet<&str> = HashSet::new();
        if let Some(defs) = self.symbol_index.get(short) {
            for (def_rel, _) in defs {
                if let Some(imps) = self.importers_index.get(def_rel) {
                    for imp in imps {
                        defining_importers.insert(imp.as_str());
                    }
                }
            }
        }

        let Some(entries) = self.callers_index.get(short) else {
            return Vec::new();
        };
        let mut ranked: Vec<(bool, GraphEdgeHit)> = Vec::with_capacity(entries.len());
        let mut seen: HashSet<(&str, usize)> = HashSet::new();
        for (rel, sym_idx) in entries {
            if !seen.insert((rel.as_str(), *sym_idx)) {
                continue;
            }
            let Some(node) = self.nodes.get(rel) else {
                continue;
            };
            let Some(sym) = node.symbols.get(*sym_idx) else {
                continue;
            };
            let imports_def = defining_importers.contains(rel.as_str());
            ranked.push((
                imports_def,
                GraphEdgeHit {
                    rel_path: rel.clone(),
                    symbol: sym.qualified.clone(),
                    kind: sym.kind.clone(),
                    start_line: sym.start_line,
                    end_line: sym.end_line,
                    exported: sym.exported,
                    is_test: sym.is_test,
                },
            ));
        }
        ranked.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.is_test.cmp(&b.1.is_test))
                .then_with(|| a.1.rel_path.cmp(&b.1.rel_path))
                .then_with(|| a.1.start_line.cmp(&b.1.start_line))
        });
        ranked.into_iter().take(limit).map(|(_, hit)| hit).collect()
    }

    pub fn callees(&self, rel_path: &str, symbol: &str, limit: usize) -> Vec<GraphEdgeHit> {
        let Some(node) = self.nodes.get(rel_path) else {
            return Vec::new();
        };
        let query = symbol.trim();
        let sym_idx = node
            .symbols
            .iter()
            .position(|s| s.qualified == query || s.name == query)
            .or_else(|| {
                let lower = query.to_ascii_lowercase();
                node.symbols
                    .iter()
                    .position(|s| s.name.to_ascii_lowercase() == lower)
            });
        let Some(sym_idx) = sym_idx else {
            return Vec::new();
        };
        let Some(callees) = node.calls_by_symbol.get(sym_idx) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for callee in callees {
            let mut resolved = false;
            if let Some(defs) = self.symbol_index.get(callee) {
                for (def_rel, def_idx) in defs {
                    if let Some(def_node) = self.nodes.get(def_rel) {
                        if let Some(sym) = def_node.symbols.get(*def_idx) {
                            let key = (def_rel.clone(), sym.start_line);
                            if seen.insert(key) {
                                out.push(GraphEdgeHit {
                                    rel_path: def_rel.clone(),
                                    symbol: sym.qualified.clone(),
                                    kind: sym.kind.clone(),
                                    start_line: sym.start_line,
                                    end_line: sym.end_line,
                                    exported: sym.exported,
                                    is_test: sym.is_test,
                                });
                                resolved = true;
                            }
                        }
                    }
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
            if !resolved && seen.insert((String::new(), out.len() + 100_000)) {
                out.push(GraphEdgeHit {
                    rel_path: String::new(),
                    symbol: callee.clone(),
                    kind: "call".to_owned(),
                    start_line: 0,
                    end_line: 0,
                    exported: false,
                    is_test: false,
                });
                if out.len() >= limit {
                    return out;
                }
            }
        }
        out
    }

    pub fn tests_covering(&self, rel_path: &str, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }
        let mut tests: BTreeSet<String> = BTreeSet::new();
        if let Some(node) = self.nodes.get(rel_path) {
            if node.symbols.iter().any(|s| s.is_test) || node.is_test_file {
                tests.insert(rel_path.to_owned());
            }
            if let Some(importers) = self.importers_index.get(rel_path) {
                for imp in importers {
                    if self.nodes.get(imp).is_some_and(|n| n.is_test_file) {
                        tests.insert(imp.clone());
                    }
                }
            }
            let stem = Path::new(rel_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !stem.is_empty() {
                for (cand_rel, cand_node) in &self.nodes {
                    if cand_node.is_test_file
                        && (cand_rel.contains(&format!("{stem}_test"))
                            || cand_rel.contains(&format!("{stem}.test"))
                            || cand_rel.contains(&format!("{stem}.spec"))
                            || cand_rel.contains(&format!("test_{stem}")))
                    {
                        tests.insert(cand_rel.clone());
                    }
                }
            }
            for sym in &node.symbols {
                if !sym.exported || sym.is_test {
                    continue;
                }
                if let Some(callers) = self.callers_index.get(&sym.name) {
                    for (caller_rel, caller_idx) in callers {
                        if let Some(caller_node) = self.nodes.get(caller_rel) {
                            let is_test = caller_node.is_test_file
                                || caller_node
                                    .symbols
                                    .get(*caller_idx)
                                    .is_some_and(|s| s.is_test);
                            if is_test {
                                tests.insert(caller_rel.clone());
                            }
                        }
                    }
                }
            }
        }
        tests.into_iter().take(limit).collect()
    }

    pub fn impact(&self, target: &str, depth: usize, limit: usize) -> ImpactReport {
        let depth = depth.clamp(1, 2);
        let target_trimmed = target.trim();
        let mut report = ImpactReport {
            target: target_trimmed.to_owned(),
            ..ImpactReport::default()
        };
        if target_trimmed.is_empty() || limit == 0 {
            return report;
        }

        let mut seed_files: Vec<String> = Vec::new();
        let mut seed_symbols: Vec<String> = Vec::new();

        if self.nodes.contains_key(target_trimmed) {
            seed_files.push(target_trimmed.to_owned());
            if let Some(node) = self.nodes.get(target_trimmed) {
                for sym in node.symbols.iter().filter(|s| s.exported && !s.is_test).take(8) {
                    seed_symbols.push(sym.name.clone());
                    report.definitions.push(GraphEdgeHit {
                        rel_path: target_trimmed.to_owned(),
                        symbol: sym.qualified.clone(),
                        kind: sym.kind.clone(),
                        start_line: sym.start_line,
                        end_line: sym.end_line,
                        exported: sym.exported,
                        is_test: sym.is_test,
                    });
                }
            }
        } else {
            let defs = self.definitions(target_trimmed, limit);
            for d in &defs {
                if !seed_files.contains(&d.rel_path) {
                    seed_files.push(d.rel_path.clone());
                }
            }
            seed_symbols.push(target_trimmed.to_owned());
            report.definitions = defs;
        }

        // BFS over importers up to `depth` hops.
        let mut visited_files: HashSet<String> = seed_files.iter().cloned().collect();
        let mut queue: VecDeque<(String, usize)> =
            seed_files.iter().cloned().map(|f| (f, 0)).collect();
        let mut importers_out = Vec::new();
        let mut tests_set: BTreeSet<String> = BTreeSet::new();
        for f in &seed_files {
            for t in self.tests_covering(f, limit) {
                tests_set.insert(t);
            }
        }
        while let Some((file, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            if let Some(imps) = self.importers_index.get(&file) {
                for imp in imps {
                    if visited_files.insert(imp.clone()) {
                        if self.nodes.get(imp).is_some_and(|n| n.is_test_file) {
                            tests_set.insert(imp.clone());
                        } else if importers_out.len() < limit {
                            importers_out.push(imp.clone());
                        }
                        if d + 1 < depth {
                            queue.push_back((imp.clone(), d + 1));
                        }
                    }
                }
            }
        }
        report.importers = importers_out;

        let mut callers_out = Vec::new();
        let mut seen_callers = HashSet::new();
        for sym in seed_symbols {
            for hit in self.callers(&sym, limit) {
                if hit.is_test {
                    tests_set.insert(hit.rel_path.clone());
                    continue;
                }
                if seen_callers.insert((hit.rel_path.clone(), hit.start_line)) {
                    callers_out.push(hit);
                    if callers_out.len() >= limit {
                        break;
                    }
                }
            }
            if callers_out.len() >= limit {
                break;
            }
        }
        report.callers = callers_out;
        report.tests_covering = tests_set.into_iter().take(limit).collect();
        report
    }

    /// Generates a compact, strictly budgeted Query-Aware Context Hint for Turn 1.
    pub fn query_context_hint<F>(
        &self,
        prompt: &str,
        trigram_hits: F,
        canonical_root: &Path,
        max_bytes: usize,
    ) -> Option<String>
    where
        F: Fn(&str) -> Option<Vec<PathBuf>>,
    {
        if !self.ready || self.nodes.is_empty() || max_bytes < 128 {
            return None;
        }
        let terms = extract_query_terms(prompt);
        if terms.is_empty() {
            return None;
        }
        let prompt_lower = prompt.to_ascii_lowercase();
        let wants_tests = prompt_lower.contains("test")
            || prompt_lower.contains("teste")
            || prompt_lower.contains("spec");

        let mut scores: HashMap<&str, f64> = HashMap::new();
        let mut matched_terms_per_file: HashMap<&str, Vec<String>> = HashMap::new();

        for term in &terms {
            let lower = term.to_ascii_lowercase();

            // 1) Exact / lowercase symbol hits (+10 exact)
            if let Some(defs) = self.symbol_lower_index.get(&lower) {
                for (rel, sym_idx) in defs {
                    let is_test = self
                        .nodes
                        .get(rel.as_str())
                        .and_then(|n| n.symbols.get(*sym_idx))
                        .is_some_and(|s| s.is_test);
                    let bonus = if is_test && !wants_tests { 3.5 } else { 10.0 };
                    *scores.entry(rel.as_str()).or_insert(0.0) += bonus;
                    matched_terms_per_file
                        .entry(rel.as_str())
                        .or_default()
                        .push(term.clone());
                }
            }

            // 2) Path stem / substring match (+8 exact filename stem, +4 substring)
            for (rel, node) in &self.nodes {
                let rel_lower = rel.to_ascii_lowercase();
                let stem = Path::new(rel)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if stem == lower || rel_lower == lower || rel_lower.ends_with(&format!("/{lower}"))
                {
                    *scores.entry(rel.as_str()).or_insert(0.0) += 8.0;
                    matched_terms_per_file
                        .entry(rel.as_str())
                        .or_default()
                        .push(term.clone());
                } else if lower.len() >= 5 && rel_lower.contains(&lower) {
                    *scores.entry(rel.as_str()).or_insert(0.0) += 4.0;
                    if node.symbols.len() <= 200 {
                        matched_terms_per_file
                            .entry(rel.as_str())
                            .or_default()
                            .push(term.clone());
                    }
                }
            }

            // 3) Trigram IDF recall
            if term.len() >= 4 {
                if let Some(abs_hits) = trigram_hits(term) {
                    if !abs_hits.is_empty() && abs_hits.len() <= 160 {
                        let idf = 3.5 / (2.0 + abs_hits.len() as f64).log2();
                        for abs in abs_hits {
                            if let Ok(rel) = abs.strip_prefix(canonical_root) {
                                let rel_str = rel.to_string_lossy().replace('\\', "/");
                                if let Some((stored_rel, _)) = self.nodes.get_key_value(&rel_str) {
                                    *scores.entry(stored_rel.as_str()).or_insert(0.0) += idf;
                                }
                            }
                        }
                    }
                }
            }
        }

        if scores.is_empty() {
            return None;
        }

        let mut ranked: Vec<(&str, f64)> = scores
            .into_iter()
            .map(|(rel, sc)| {
                let is_test_file = self.nodes.get(rel).is_some_and(|n| n.is_test_file);
                let adjusted = if is_test_file && !wants_tests {
                    sc * 0.35
                } else {
                    sc
                };
                (rel, adjusted)
            })
            .filter(|(_, sc)| *sc >= 4.0)
            .collect();
        if ranked.is_empty() {
            return None;
        }
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(b.0))
        });

        let mut out = String::new();
        for (rel, _) in ranked.into_iter().take(3) {
            let Some(node) = self.nodes.get(rel) else {
                continue;
            };
            let terms_for_file = matched_terms_per_file.get(rel);
            let mut chosen_syms: Vec<&GraphSymbol> = Vec::new();

            // Prefer symbols whose name matches any query term.
            if let Some(file_terms) = terms_for_file {
                for sym in &node.symbols {
                    if !wants_tests && sym.is_test {
                        continue;
                    }
                    let sym_l = sym.name.to_ascii_lowercase();
                    if file_terms.iter().any(|t| {
                        let tl = t.to_ascii_lowercase();
                        sym_l == tl || sym_l.contains(&tl)
                    }) {
                        chosen_syms.push(sym);
                    }
                }
            }
            if chosen_syms.is_empty() {
                for sym in &node.symbols {
                    if sym.kind == "impl" || (!wants_tests && sym.is_test) {
                        continue;
                    }
                    chosen_syms.push(sym);
                    if chosen_syms.len() >= 4 {
                        break;
                    }
                }
            } else {
                chosen_syms.truncate(5);
            }

            let mut line = format!("- {rel}:");
            if !chosen_syms.is_empty() {
                let sym_parts: Vec<String> = chosen_syms
                    .iter()
                    .map(|s| format!("{}(L{}-L{})", s.qualified, s.start_line, s.end_line))
                    .collect();
                let _ = write!(line, " {}", sym_parts.join(", "));
            }
            if let Some(imps) = self.importers_index.get(rel) {
                let non_test_imps: Vec<&str> = imps
                    .iter()
                    .filter(|p| !self.nodes.get(p.as_str()).is_some_and(|n| n.is_test_file))
                    .take(2)
                    .map(String::as_str)
                    .collect();
                if !non_test_imps.is_empty() {
                    let _ = write!(line, " [importers: {}]", non_test_imps.join(", "));
                }
            }
            let tests = self.tests_covering(rel, 2);
            if !tests.is_empty() {
                let _ = write!(line, " [tests: {}]", tests.join(", "));
            }

            if out.len() + line.len() + usize::from(!out.is_empty()) > max_bytes {
                if out.is_empty() {
                    let truncated = truncate_utf8(&line, max_bytes);
                    out.push_str(truncated);
                }
                break;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&line);
        }

        (!out.is_empty()).then_some(out)
    }

    pub fn save_snapshot(&mut self, canonical_root: &Path) -> io::Result<()> {
        if self.importers_dirty {
            self.rebuild_importers_index();
        }
        let target = graph_snapshot_path_for(canonical_root)?;
        let Some(cache_dir) = target.parent() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no graph snapshot parent",
            ));
        };
        fs::create_dir_all(cache_dir)?;
        let tmp = target.with_extension(format!("bin.{}.tmp", std::process::id()));

        let mut nodes: Vec<FileGraphNode> = self.nodes.values().cloned().collect();
        nodes.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        let payload = serde_json::to_vec(&GraphSnapshotDisk { nodes })
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let mut buf = Vec::with_capacity(GRAPH_MAGIC.len() + payload.len());
        buf.extend_from_slice(GRAPH_MAGIC);
        buf.extend_from_slice(&payload);

        let mut file = fs::File::create(&tmp)?;
        file.write_all(&buf)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &target)?;
        self.dirty_since_save = 0;
        Ok(())
    }

    pub fn load_snapshot(canonical_root: &Path) -> io::Result<Self> {
        let path = graph_snapshot_path_for(canonical_root)?;
        let mut raw = Vec::new();
        fs::File::open(&path)?.read_to_end(&mut raw)?;
        if raw.len() < GRAPH_MAGIC.len() || &raw[..GRAPH_MAGIC.len()] != GRAPH_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid graph magic",
            ));
        }
        let disk: GraphSnapshotDisk = serde_json::from_slice(&raw[GRAPH_MAGIC.len()..])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let mut graph = Self::default();
        for node in disk.nodes {
            graph.add_to_indexes(&node);
            graph.nodes.insert(node.rel_path.clone(), node);
        }
        graph.importers_dirty = true;
        Ok(graph)
    }

    fn push_symbol_hits(&self, entries: &[(String, usize)], hits: &mut Vec<GraphEdgeHit>) {
        for (rel, sym_idx) in entries {
            if let Some(node) = self.nodes.get(rel) {
                if let Some(sym) = node.symbols.get(*sym_idx) {
                    hits.push(GraphEdgeHit {
                        rel_path: rel.clone(),
                        symbol: sym.qualified.clone(),
                        kind: sym.kind.clone(),
                        start_line: sym.start_line,
                        end_line: sym.end_line,
                        exported: sym.exported,
                        is_test: sym.is_test,
                    });
                }
            }
        }
    }

    fn add_to_indexes(&mut self, node: &FileGraphNode) {
        let rel = &node.rel_path;
        for (idx, sym) in node.symbols.iter().enumerate() {
            if sym.kind == "impl" {
                continue;
            }
            self.symbol_index
                .entry(sym.name.clone())
                .or_default()
                .push((rel.clone(), idx));
            self.symbol_lower_index
                .entry(sym.name.to_ascii_lowercase())
                .or_default()
                .push((rel.clone(), idx));
            if sym.qualified != sym.name {
                self.symbol_index
                    .entry(sym.qualified.clone())
                    .or_default()
                    .push((rel.clone(), idx));
                self.symbol_lower_index
                    .entry(sym.qualified.to_ascii_lowercase())
                    .or_default()
                    .push((rel.clone(), idx));
            }
            if let Some(calls) = node.calls_by_symbol.get(idx) {
                for callee in calls {
                    self.callers_index
                        .entry(callee.clone())
                        .or_default()
                        .push((rel.clone(), idx));
                }
            }
        }
    }

    fn remove_from_indexes(&mut self, rel_path: &str) {
        let Some(old) = self.nodes.get(rel_path) else {
            return;
        };
        for sym in &old.symbols {
            if sym.kind == "impl" {
                continue;
            }
            prune_rel_entries(&mut self.symbol_index, &sym.name, rel_path);
            prune_rel_entries(
                &mut self.symbol_lower_index,
                &sym.name.to_ascii_lowercase(),
                rel_path,
            );
            if sym.qualified != sym.name {
                prune_rel_entries(&mut self.symbol_index, &sym.qualified, rel_path);
                prune_rel_entries(
                    &mut self.symbol_lower_index,
                    &sym.qualified.to_ascii_lowercase(),
                    rel_path,
                );
            }
        }
        for calls in &old.calls_by_symbol {
            for callee in calls {
                prune_rel_entries(&mut self.callers_index, callee, rel_path);
            }
        }
    }

    fn rebuild_importers_index(&mut self) {
        let mut by_stem: HashMap<String, Vec<String>> = HashMap::new();
        for rel in self.nodes.keys() {
            if let Some(stem) = Path::new(rel).file_stem().and_then(|s| s.to_str()) {
                if !matches!(stem, "mod" | "index" | "lib" | "__init__" | "main") {
                    by_stem.entry(stem.to_owned()).or_default().push(rel.clone());
                }
            }
        }

        let mut importers: HashMap<String, BTreeSet<String>> = HashMap::new();
        for (importing_rel, node) in &self.nodes {
            if node.raw_imports.is_empty() {
                continue;
            }
            let importing_dir = Path::new(importing_rel).parent();
            for imp in &node.raw_imports {
                // 1) Relative import (`./foo`, `../bar`)
                if imp.starts_with('.') {
                    if let Some(dir) = importing_dir {
                        let joined = normalize_rel_join(dir, imp);
                        for suffix in ["", ".ts", ".tsx", ".js", ".jsx", ".rs", ".py", "/index.ts", "/index.js", "/mod.rs"] {
                            let cand = format!("{joined}{suffix}");
                            if self.nodes.contains_key(&cand) && &cand != importing_rel {
                                importers
                                    .entry(cand)
                                    .or_default()
                                    .insert(importing_rel.clone());
                            }
                        }
                    }
                    continue;
                }
                // 2) Suffix/segment resolution (`crate::tools::outline` or `crate::engine::Planner` -> `src/engine.rs`)
                let slash_imp = imp.replace("::", "/").replace('.', "/");
                let mut candidates_to_check: Vec<&str> = Vec::with_capacity(2);
                let tail = slash_imp.rsplit('/').next().unwrap_or(&slash_imp);
                if !tail.is_empty() {
                    candidates_to_check.push(tail);
                }
                if let Some((parent_imp, _)) = slash_imp.rsplit_once('/') {
                    let parent_tail = parent_imp.rsplit('/').next().unwrap_or(parent_imp);
                    if !parent_tail.is_empty() && parent_tail != tail {
                        candidates_to_check.push(parent_tail);
                    }
                }
                for cand_stem in &candidates_to_check {
                    if let Some(cands) = by_stem.get(*cand_stem) {
                        for target_rel in cands {
                            if target_rel == importing_rel {
                                continue;
                            }
                            if cands.len() == 1 || path_matches_import(target_rel, &slash_imp) {
                                importers
                                    .entry(target_rel.clone())
                                    .or_default()
                                    .insert(importing_rel.clone());
                            }
                        }
                    }
                    // Check `dir/mod.rs` or `dir/index.ts` when segment matches directory name.
                    for idx_file in ["mod.rs", "index.ts", "index.tsx", "index.js", "__init__.py"] {
                        let cand_suffix = format!("/{cand_stem}/{idx_file}");
                        for target_rel in self.nodes.keys() {
                            if target_rel != importing_rel && target_rel.ends_with(&cand_suffix) {
                                importers
                                    .entry(target_rel.clone())
                                    .or_default()
                                    .insert(importing_rel.clone());
                            }
                        }
                    }
                }
            }
        }

        self.importers_index = importers
            .into_iter()
            .map(|(k, set)| (k, set.into_iter().collect()))
            .collect();
        self.importers_dirty = false;
    }
}

fn prune_rel_entries(map: &mut HashMap<String, Vec<(String, usize)>>, key: &str, rel_path: &str) {
    if let Some(list) = map.get_mut(key) {
        list.retain(|(r, _)| r != rel_path);
        if list.is_empty() {
            map.remove(key);
        }
    }
}

fn graph_snapshot_path_for(canonical_root: &Path) -> io::Result<PathBuf> {
    cache_subdir_path("graphs", "slim-graph-root-v1\n", canonical_root, "v1.bin")
}

fn normalize_rel_join(base: &Path, rel: &str) -> String {
    let mut parts: Vec<String> = base
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(os) => os.to_str().map(str::to_owned),
            _ => None,
        })
        .collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s.to_owned()),
        }
    }
    parts.join("/")
}

fn path_matches_import(target_rel: &str, slash_imp: &str) -> bool {
    let without_ext = target_rel
        .rsplit_once('.')
        .map_or(target_rel, |(head, _)| head);
    without_ext.ends_with(slash_imp)
        || slash_imp.ends_with(without_ext)
        || slash_imp
            .split('/')
            .rev()
            .take(2)
            .all(|seg| without_ext.contains(seg))
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn is_test_path(rel_path: &str) -> bool {
    let lower = rel_path.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.contains("/test/")
        || lower.contains("/__tests__/")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_tests.rs")
        || lower.ends_with("_test.go")
        || lower.ends_with("_test.py")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".test.tsx")
        || lower.ends_with(".spec.ts")
        || lower.ends_with(".spec.tsx")
        || lower.ends_with(".test.js")
        || lower.ends_with(".spec.js")
        || Path::new(&lower)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("test_"))
}

fn parse_file_node(
    rel_path: &str,
    len: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
    text: &str,
) -> Option<FileGraphNode> {
    let lang = OutlineLang::from_path(Path::new(rel_path))?;
    if matches!(lang, OutlineLang::Markdown | OutlineLang::Toml) {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let outline = extract_outline_lines(lang, &lines);
    let is_test_file = is_test_path(rel_path);
    let raw_imports = extract_imports(lang, &lines);

    // Track Rust #[cfg(test)] modules so items inside are flagged as test code.
    let cfg_test_ranges: Vec<(usize, usize)> = if lang == OutlineLang::Rust {
        outline
            .iter()
            .filter(|s| {
                s.kind == "mod"
                    && lines
                        .get(s.start_line.saturating_sub(2)..s.start_line)
                        .is_some_and(|ctx| ctx.iter().any(|l| l.contains("cfg(test)")))
            })
            .map(|s| (s.start_line, s.end_line))
            .collect()
    } else {
        Vec::new()
    };

    let mut symbols = Vec::with_capacity(outline.len());
    let mut calls_by_symbol = Vec::with_capacity(outline.len());

    // Stack of (container_name, end_line) to construct qualified names.
    let mut parents: Vec<(String, usize)> = Vec::new();

    for sym in &outline {
        while parents
            .last()
            .is_some_and(|(_, end_line)| sym.start_line > *end_line)
        {
            parents.pop();
        }
        let container_name = if sym.kind == "impl" {
            extract_impl_target_name(&sym.name)
        } else if matches!(
            sym.kind,
            "class" | "struct" | "enum" | "trait" | "interface" | "mod" | "namespace"
        ) {
            sym.name.clone()
        } else {
            String::new()
        };

        let qualified = if sym.kind == "impl" {
            sym.name.clone()
        } else if let Some((parent, _)) = parents.last() {
            let sep = if matches!(lang, OutlineLang::Rust | OutlineLang::CFamily) {
                "::"
            } else {
                "."
            };
            format!("{parent}{sep}{}", sym.name)
        } else {
            sym.name.clone()
        };

        let is_test = is_test_file
            || cfg_test_ranges
                .iter()
                .any(|(st, en)| sym.start_line >= *st && sym.end_line <= *en)
            || has_test_marker(lang, &lines, sym.start_line, &sym.name);
        let exported = !is_test && is_symbol_exported(lang, &sym.signature, &sym.name, rel_path);

        let calls = if matches!(sym.kind, "fn" | "method") {
            let start_idx = sym.start_line.saturating_sub(1);
            let end_idx = sym.end_line.min(lines.len());
            extract_calls(&lines[start_idx..end_idx], &sym.name)
        } else {
            Vec::new()
        };

        symbols.push(GraphSymbol {
            kind: sym.kind.to_owned(),
            name: sym.name.clone(),
            qualified,
            start_line: sym.start_line,
            end_line: sym.end_line,
            exported,
            is_test,
        });
        calls_by_symbol.push(calls);

        if !container_name.is_empty() {
            parents.push((container_name, sym.end_line));
        }
    }

    Some(FileGraphNode {
        rel_path: rel_path.to_owned(),
        len,
        mtime_secs,
        mtime_nanos,
        is_test_file,
        symbols,
        raw_imports,
        calls_by_symbol,
    })
}

fn extract_impl_target_name(raw_impl: &str) -> String {
    // Handles `<T> Foo<T>`, `Trait for Foo<T>`, `Foo`.
    let after_for = raw_impl
        .split(" for ")
        .last()
        .unwrap_or(raw_impl)
        .trim();
    let without_generics = if after_for.starts_with('<') {
        after_for
            .split_once('>')
            .map_or(after_for, |(_, rest)| rest.trim())
    } else {
        after_for
    };
    let head = without_generics
        .split('<')
        .next()
        .unwrap_or(without_generics)
        .trim();
    head.rsplit("::").next().unwrap_or(head).to_owned()
}

fn has_test_marker(lang: OutlineLang, lines: &[&str], start_line: usize, name: &str) -> bool {
    match lang {
        OutlineLang::Rust => {
            let from = start_line.saturating_sub(4);
            let to = start_line.saturating_sub(1);
            lines.get(from..to).is_some_and(|ctx| {
                ctx.iter()
                    .any(|l| l.contains("#[test]") || l.contains("::test]") || l.contains("#[rstest"))
            })
        }
        OutlineLang::Python => name.starts_with("test_"),
        OutlineLang::Go => name.starts_with("Test") || name.starts_with("Benchmark"),
        _ => false,
    }
}

fn is_symbol_exported(lang: OutlineLang, signature: &str, name: &str, rel_path: &str) -> bool {
    match lang {
        OutlineLang::Rust => {
            signature.starts_with("pub ") || signature.starts_with("pub(")
        }
        OutlineLang::TsJs => {
            signature.starts_with("export ")
                || (!signature.starts_with("private ") && signature.starts_with("public "))
        }
        OutlineLang::Python => !name.starts_with('_'),
        OutlineLang::Go => name.chars().next().is_some_and(|c| c.is_ascii_uppercase()),
        OutlineLang::CFamily => {
            rel_path.ends_with(".h")
                || rel_path.ends_with(".hpp")
                || !signature.starts_with("static ")
        }
        OutlineLang::JavaSwift => !signature.starts_with("private "),
        OutlineLang::Markdown | OutlineLang::Toml => false,
    }
}

fn extract_imports(lang: OutlineLang, lines: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |item: String| {
        let trimmed = item.trim().to_owned();
        if !trimmed.is_empty() && seen.insert(trimmed.clone()) {
            out.push(trimmed);
        }
    };

    for line in lines.iter().take(256) {
        let t = line.trim();
        match lang {
            OutlineLang::Rust => {
                if let Some(rest) = t.strip_prefix("use ") {
                    let core = rest.trim_end_matches(';').split(" as ").next().unwrap_or(rest);
                    let base = core.split('{').next().unwrap_or(core).trim_end_matches("::");
                    push(base.to_owned());
                } else if let Some(rest) = t.strip_prefix("mod ") {
                    if let Some(name) = rest.strip_suffix(';') {
                        push(name.trim().to_owned());
                    }
                }
            }
            OutlineLang::TsJs => {
                if (t.starts_with("import ") || t.starts_with("export ")) && t.contains(" from ") {
                    if let Some((_, rhs)) = t.rsplit_once(" from ") {
                        let q = rhs.trim().trim_end_matches(';').trim_matches(|c| c == '\'' || c == '"');
                        push(q.to_owned());
                    }
                } else if let Some(idx) = t.find("require(") {
                    let after = &t[idx + "require(".len()..];
                    if let Some((arg, _)) = after.split_once(')') {
                        let q = arg.trim().trim_matches(|c| c == '\'' || c == '"');
                        push(q.to_owned());
                    }
                }
            }
            OutlineLang::Python => {
                if let Some(rest) = t.strip_prefix("from ") {
                    if let Some((module, _)) = rest.split_once(" import ") {
                        push(module.trim().to_owned());
                    }
                } else if let Some(rest) = t.strip_prefix("import ") {
                    let module = rest.split(',').next().unwrap_or(rest).split(" as ").next().unwrap_or(rest);
                    push(module.trim().to_owned());
                }
            }
            OutlineLang::Go => {
                if let Some(rest) = t.strip_prefix("import ") {
                    let q = rest.trim().trim_matches('"');
                    if !q.starts_with('(') {
                        push(q.to_owned());
                    }
                } else if t.starts_with('"') && t.ends_with('"') && t.len() > 2 {
                    push(t.trim_matches('"').to_owned());
                }
            }
            OutlineLang::CFamily => {
                if let Some(rest) = t.strip_prefix("#include ") {
                    let q = rest.trim().trim_matches(|c| c == '"' || c == '<' || c == '>');
                    push(q.to_owned());
                }
            }
            OutlineLang::JavaSwift => {
                if let Some(rest) = t.strip_prefix("import ") {
                    push(rest.trim_end_matches(';').to_owned());
                }
            }
            OutlineLang::Markdown | OutlineLang::Toml => {}
        }
    }
    out
}

fn is_noisy_ident(ident: &str) -> bool {
    matches!(
        ident,
        "if" | "for"
            | "while"
            | "match"
            | "switch"
            | "return"
            | "sizeof"
            | "typeof"
            | "fn"
            | "func"
            | "def"
            | "class"
            | "struct"
            | "enum"
            | "Some"
            | "None"
            | "Ok"
            | "Err"
            | "new"
            | "get"
            | "set"
            | "len"
            | "is_empty"
            | "clone"
            | "default"
            | "to_string"
            | "to_owned"
            | "into"
            | "from"
            | "unwrap"
            | "expect"
            | "ok"
            | "err"
            | "iter"
            | "into_iter"
            | "map"
            | "filter"
            | "collect"
            | "push"
            | "pop"
            | "insert"
            | "remove"
            | "contains"
            | "as_str"
            | "as_ref"
            | "format"
            | "println"
            | "eprintln"
            | "vec"
            | "log"
            | "warn"
            | "error"
            | "debug"
            | "info"
            | "assert"
            | "assert_eq"
            | "assert_ne"
    )
}

fn extract_calls(body_lines: &[&str], own_name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for line in body_lines {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with("/*") {
            continue;
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        let mut in_quote: Option<u8> = None;
        while i < bytes.len() {
            if let Some(q) = in_quote {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == q {
                    in_quote = None;
                }
                i += 1;
                continue;
            }
            if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
                break;
            }
            if bytes[i] == b'"' || bytes[i] == b'`' {
                in_quote = Some(bytes[i]);
                i += 1;
                continue;
            }
            if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let mut k = i;
                while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if bytes.get(k) == Some(&b'(') {
                    let ident = &line[start..i];
                    if ident != own_name && ident.len() >= 3 && !is_noisy_ident(ident) {
                        if seen.insert(ident.to_owned()) {
                            out.push(ident.to_owned());
                            if out.len() >= MAX_CALLEES_PER_SYMBOL {
                                return out;
                            }
                        }
                    }
                }
                continue;
            }
            i += 1;
        }
    }
    out
}

fn is_stopword(lower: &str) -> bool {
    matches!(
        lower,
        "para"
            | "como"
            | "onde"
            | "aqui"
            | "isso"
            | "isto"
            | "esse"
            | "essa"
            | "este"
            | "esta"
            | "pelo"
            | "pela"
            | "mais"
            | "menos"
            | "sobre"
            | "entre"
            | "antes"
            | "depois"
            | "agora"
            | "pode"
            | "quero"
            | "fazer"
            | "preciso"
            | "vamos"
            | "olha"
            | "verifique"
            | "corrija"
            | "implemente"
            | "criar"
            | "editar"
            | "about"
            | "with"
            | "from"
            | "that"
            | "this"
            | "what"
            | "when"
            | "where"
            | "which"
            | "have"
            | "should"
            | "could"
            | "would"
            | "please"
            | "check"
            | "make"
            | "into"
            | "only"
            | "some"
            | "more"
            | "code"
            | "file"
            | "files"
            | "line"
            | "lines"
            | "repo"
            | "project"
            | "function"
            | "method"
            | "struct"
            | "class"
    )
}

fn extract_query_terms(prompt: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut seen = HashSet::new();

    for tok in prompt.split_whitespace() {
        let clean = tok.trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == ',' || c == ';');
        let push_term = |raw: &str, terms: &mut Vec<String>, seen: &mut HashSet<String>| {
            if terms.len() >= 10 {
                return;
            }
            let trimmed = raw.trim_matches(|c: char| {
                !c.is_ascii_alphanumeric() && c != '_' && c != '.' && c != '/' && c != '-'
            });
            if trimmed.len() < 4 {
                return;
            }
            let lower = trimmed.to_ascii_lowercase();
            if is_stopword(&lower) {
                return;
            }
            if seen.insert(lower) {
                terms.push(trimmed.to_owned());
            }
        };
        if clean.contains("::") {
            for part in clean.split("::") {
                push_term(part, &mut terms, &mut seen);
            }
        } else if clean.contains('/') {
            push_term(clean, &mut terms, &mut seen);
            if let Some(stem) = Path::new(clean).file_stem().and_then(|s| s.to_str()) {
                push_term(stem, &mut terms, &mut seen);
            }
        } else {
            let head = clean.split('(').next().unwrap_or(clean);
            push_term(head, &mut terms, &mut seen);
        }
        if terms.len() >= 10 {
            break;
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "slim-graph-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn extracts_definitions_imports_callers_callees_tests_and_impact() {
        let mut graph = SemanticGraph::default();
        let engine_src = r#"
pub struct Planner {
    limit: usize,
}

impl Planner {
    pub fn build_plan(&self) -> usize {
        helper_compute(self.limit)
    }
}

fn helper_compute(v: usize) -> usize {
    v + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_works() {
        let p = Planner { limit: 3 };
        assert_eq!(p.build_plan(), 4);
    }
}
"#;
        let caller_src = r#"
use crate::engine::Planner;

pub fn execute_turn() -> usize {
    let p = Planner { limit: 10 };
    p.build_plan()
}
"#;
        let test_src = r#"
use crate::engine::Planner;

fn integration_builds_plan() {
    let p = Planner { limit: 5 };
    let _ = p.build_plan();
}
"#;

        graph.upsert_bytes("src/engine.rs", engine_src.len() as u64, 1, 0, engine_src.as_bytes());
        graph.upsert_bytes("src/runner.rs", caller_src.len() as u64, 1, 0, caller_src.as_bytes());
        graph.upsert_bytes(
            "tests/engine_test.rs",
            test_src.len() as u64,
            1,
            0,
            test_src.as_bytes(),
        );
        graph.mark_ready();

        let defs = graph.definitions("build_plan", 8);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].rel_path, "src/engine.rs");
        assert_eq!(defs[0].symbol, "Planner::build_plan");
        assert!(defs[0].exported);

        let callers = graph.callers("build_plan", 8);
        assert!(callers.iter().any(|c| c.rel_path == "src/runner.rs" && c.symbol == "execute_turn"));

        let callees = graph.callees("src/engine.rs", "build_plan", 8);
        assert!(callees.iter().any(|c| c.symbol == "helper_compute" && c.rel_path == "src/engine.rs"));

        let (_imports, importers) = graph.imports_and_importers("src/engine.rs");
        assert!(importers.contains(&"src/runner.rs".to_owned()));

        let tests = graph.tests_covering("src/engine.rs", 8);
        assert!(tests.contains(&"src/engine.rs".to_owned()));
        assert!(tests.contains(&"tests/engine_test.rs".to_owned()));

        let impact = graph.impact("src/engine.rs", 2, 16);
        assert!(impact.importers.contains(&"src/runner.rs".to_owned()));
        assert!(impact.tests_covering.contains(&"tests/engine_test.rs".to_owned()));
    }

    #[test]
    fn persists_atomically_and_generates_budgeted_query_hint() {
        let dir = make_temp_dir("persist");
        let canon = fs::canonicalize(&dir).unwrap();
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        let app_file = src.join("poll_engine.rs");
        let body = "pub fn poll_changes(limit: usize) -> usize {\n    limit * 2\n}\n";
        fs::write(&app_file, body).unwrap();

        let mut graph = SemanticGraph::default();
        graph.upsert_bytes("src/poll_engine.rs", body.len() as u64, 1, 0, body.as_bytes());
        graph.mark_ready();
        graph.save_snapshot(&canon).expect("save graph snapshot");

        let mut reloaded = SemanticGraph::load_snapshot(&canon).expect("load graph snapshot");
        reloaded.mark_ready();
        assert_eq!(reloaded.file_count(), 1);
        assert_eq!(reloaded.symbol_count(), 1);

        // Noisy chat prompt without technical identifiers must yield None.
        assert!(reloaded
            .query_context_hint("oi tudo bem por favor", |_| None, &canon, 1_100)
            .is_none());

        // Specific symbol prompt produces exact file + line span within budget.
        let hint = reloaded
            .query_context_hint(
                "Por favor investigue poll_changes em src/poll_engine.rs",
                |_| None,
                &canon,
                1_100,
            )
            .expect("query hint");
        assert!(hint.len() <= 1_100);
        assert!(hint.contains("src/poll_engine.rs"));
        assert!(hint.contains("poll_changes(L1-L3)"));

        // Incremental update reflects new symbol name.
        let new_body = "pub fn refresh_cycle() -> usize { 7 }\n";
        reloaded.upsert_bytes(
            "src/poll_engine.rs",
            new_body.len() as u64,
            2,
            0,
            new_body.as_bytes(),
        );
        assert!(reloaded.definitions("poll_changes", 4).is_empty());
        assert_eq!(reloaded.definitions("refresh_cycle", 4).len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
