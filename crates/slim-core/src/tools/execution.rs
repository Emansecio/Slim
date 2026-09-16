use super::{
    code_intel::parse_prepared_code_intel_request, code_intel::CodeIntelRequest,
    resolve_workspace_path_from_root, PresentationBudget, ToolCacheability, ToolDependencyScope,
    ToolEffectClass, ToolError, ToolOperationalSpec, ToolPresentation, ToolReplayPolicy,
    ToolResult, ToolVolatility, DEFAULT_MAX_ENTRIES, DEFAULT_MAX_HITS, MAX_ENTRIES_CAP,
    MAX_HITS_CAP, MAX_MUTATING_FILE_BYTES, MAX_READ_LINES_CAP, MAX_SEARCH_PATTERNS,
};
use crate::codeintel::DEFAULT_CODE_INTEL_LIMIT;
use crate::OperatingMode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::{File, Metadata};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub(crate) enum ToolPresentationSource {
    Read {
        prefix: String,
        full: String,
        first: usize,
        records: Vec<String>,
        next_offset: Option<usize>,
    },
    List {
        prefix: String,
        page: super::list::ListPage,
        display_root: PathBuf,
    },
    Search {
        prefix: String,
        page: super::search::SearchBatchPage,
        display_root: PathBuf,
    },
    CodeIntel {
        prefix: String,
        presentation: CodeIntelPresentation,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct CodeIntelPresentation {
    pub(crate) full: String,
    pub(crate) header: String,
    pub(crate) records: Vec<String>,
    pub(crate) header_kind: CodeIntelHeaderKind,
    pub(crate) continuation: Option<CodeIntelContinuation>,
    pub(crate) meta: String,
}

#[derive(Clone, Debug)]
pub(crate) enum CodeIntelHeaderKind {
    Static,
    References {
        symbol: Option<String>,
        total: usize,
        file_count: usize,
        completeness: String,
    },
    Symbols {
        document: bool,
        query: Option<String>,
        total: Option<usize>,
    },
    Diagnostics {
        published: bool,
        total: Option<usize>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct CodeIntelContinuation {
    /// Starting result offset echoed by the semantic backend. It lets a
    /// budgeted final page derive a continuation when the backend omits
    /// `next_offset` but still binds the page to a revision.
    pub(crate) offset: Option<usize>,
    pub(crate) next_offset: Option<usize>,
    pub(crate) revision: Option<u64>,
    pub(crate) scan_notice: Option<String>,
}

impl ToolPresentationSource {
    /// Bytes the unconstrained projection needs. Can exceed the redacted
    /// `result.output` length when raw records carry sensitive values, so
    /// the presentation planner must size its allowance by this, not by the
    /// post-redaction output.
    pub(crate) fn full_len(&self) -> usize {
        match self {
            Self::Read { full, .. } => full.len(),
            Self::List {
                page, display_root, ..
            } => page.present(usize::MAX, display_root).text.len(),
            Self::Search {
                page, display_root, ..
            } => page.present(usize::MAX, display_root).text.len(),
            Self::CodeIntel { presentation, .. } => presentation.full.len(),
        }
    }

    pub(crate) fn present(&self, budget: PresentationBudget) -> ToolPresentation {
        match self {
            Self::Read {
                prefix,
                full,
                first,
                records,
                next_offset,
            } => present_prefixed(prefix, budget, |remaining| {
                if full.len() <= remaining.max_bytes {
                    ToolPresentation::complete(full.clone())
                } else {
                    present_read_records(*first, records, *next_offset, remaining)
                }
            }),
            Self::List {
                prefix,
                page,
                display_root,
            } => present_prefixed(prefix, budget, |remaining| {
                page.present(remaining.max_bytes, display_root)
            }),
            Self::Search {
                prefix,
                page,
                display_root,
            } => present_prefixed(prefix, budget, |remaining| {
                page.present(remaining.max_bytes, display_root)
            }),
            Self::CodeIntel {
                prefix,
                presentation,
            } => present_prefixed(prefix, budget, |remaining| presentation.present(remaining)),
        }
    }

    pub(crate) fn with_prefix(mut self, prefix: String) -> Self {
        if prefix.is_empty() {
            return self;
        }
        match &mut self {
            Self::Read {
                prefix: current, ..
            }
            | Self::List {
                prefix: current, ..
            }
            | Self::Search {
                prefix: current, ..
            }
            | Self::CodeIntel {
                prefix: current, ..
            } => current.push_str(&prefix),
        }
        self
    }

    pub(crate) fn replace_prefix(mut self, from: String, to: String) -> Self {
        match &mut self {
            Self::Read { prefix, .. }
            | Self::List { prefix, .. }
            | Self::Search { prefix, .. }
            | Self::CodeIntel { prefix, .. } => {
                if !from.is_empty() && prefix.starts_with(&from) {
                    prefix.drain(..from.len());
                }
                if !to.is_empty() {
                    prefix.insert_str(0, &to);
                }
            }
        }
        self
    }
}

fn present_prefixed(
    prefix: &str,
    budget: PresentationBudget,
    present: impl FnOnce(PresentationBudget) -> ToolPresentation,
) -> ToolPresentation {
    let remaining = PresentationBudget {
        max_bytes: budget.max_bytes.saturating_sub(prefix.len()),
    };
    let mut result = present(remaining);
    if !prefix.is_empty() {
        result.text.insert_str(0, prefix);
    }
    result
}

fn present_read_records(
    first: usize,
    records: &[String],
    next_offset: Option<usize>,
    budget: PresentationBudget,
) -> ToolPresentation {
    let render = |count: usize| {
        let mut text = records[..count].concat();
        let needs_continuation = count < records.len() || next_offset.is_some();
        if needs_continuation {
            let cursor = first.saturating_add(count);
            let last = cursor.saturating_sub(1);
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!(
                "[showing lines {first}-{last}; more content available; pass \"offset\": {cursor} for the next page]"
            ));
            if let Some(next) = next_offset {
                debug_assert_eq!(next, first.saturating_add(records.len()));
            }
        }
        text
    };
    let full = render(records.len());
    if full.len() <= budget.max_bytes {
        return ToolPresentation {
            text: full,
            delivered_records: records.len(),
            complete: next_offset.is_none(),
            oversized_record: false,
        };
    }
    let mut low = 0usize;
    let mut high = records.len();
    while low < high {
        let count = low + (high - low).div_ceil(2);
        if render(count).len() <= budget.max_bytes {
            low = count;
        } else {
            high = count - 1;
        }
    }
    let mut text = render(low);
    if low == 0 {
        text.push_str(&format!(
            "\n[line and continuation exceed presentation budget; no line delivered; pass \"offset\": {first} or request a narrower page; not safe for patch.expected]"
        ));
    }
    ToolPresentation {
        text,
        delivered_records: low,
        complete: false,
        oversized_record: low == 0,
    }
}

impl CodeIntelPresentation {
    fn present(&self, budget: PresentationBudget) -> ToolPresentation {
        if self.full.len() <= budget.max_bytes {
            return ToolPresentation::complete(self.full.clone());
        }
        let render = |count: usize| {
            let mut text = self.header_for_count(count);
            for record in &self.records[..count] {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(record);
            }
            if let Some(continuation) = self.continuation_for_count(count) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&continuation);
            }
            if count < self.records.len() && self.continuation.is_none() {
                if !text.is_empty() {
                    text.push('\n');
                }
                if count == 0 {
                    text.push_str("[semantic item and metadata exceed presentation budget; no item delivered; request a narrower page; not safe for patch.expected]");
                } else {
                    text.push_str("[semantic items omitted by presentation budget; request a narrower page; not safe for patch.expected]");
                }
            }
            if !self.meta.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&self.meta);
            }
            text
        };
        let full = render(self.records.len());
        if full.len() <= budget.max_bytes {
            return ToolPresentation {
                text: full,
                delivered_records: self.records.len(),
                complete: self.continuation.is_none(),
                oversized_record: false,
            };
        }
        let mut low = 0usize;
        let mut high = self.records.len();
        while low < high {
            let count = low + (high - low).div_ceil(2);
            if render(count).len() <= budget.max_bytes {
                low = count;
            } else {
                high = count - 1;
            }
        }
        let mut text = render(low);
        if low == 0 && self.records.is_empty() {
            text.push_str("\n[semantic item and metadata exceed presentation budget; no item delivered; request a narrower page; not safe for patch.expected]");
        }
        ToolPresentation {
            text,
            delivered_records: low,
            complete: false,
            oversized_record: low == 0,
        }
    }

    fn header_for_count(&self, count: usize) -> String {
        match &self.header_kind {
            CodeIntelHeaderKind::Static => self.header.clone(),
            CodeIntelHeaderKind::References {
                symbol,
                total,
                file_count,
                completeness,
            } => match symbol {
                Some(symbol) => format!(
                    "code_intel references: {symbol} - {count} of {total} across {file_count} file(s) | {completeness}"
                ),
                None => format!(
                    "code_intel references: {count} of {total} across {file_count} file(s) | {completeness}"
                ),
            },
            CodeIntelHeaderKind::Symbols {
                document,
                query,
                total,
            } => match (document, total) {
                (true, Some(total)) => format!(
                    "code_intel action=symbol (document): {} of {} shown",
                    count, total
                ),
                (true, None) => format!("code_intel action=symbol (document): {count} shown"),
                (false, Some(total)) => format!(
                    "code_intel action=symbol (workspace): {} - {} of {} shown",
                    query.as_deref().unwrap_or(""), count, total
                ),
                (false, None) => format!(
                    "code_intel action=symbol (workspace): {} - {count} shown",
                    query.as_deref().unwrap_or("")
                ),
            },
            CodeIntelHeaderKind::Diagnostics { published, total } => {
                let counts = match total {
                    Some(total) => format!("{count} shown of {total}"),
                    None => format!("{count} shown; total unknown"),
                };
                if *published {
                    format!("code_intel diagnostics (published cache): {counts}")
                } else {
                    format!("code_intel diagnostics: {counts}")
                }
            }
        }
    }

    fn continuation_for_count(&self, count: usize) -> Option<String> {
        let continuation = self.continuation.as_ref()?;
        let next = continuation.next_offset.map_or_else(
            || {
                (count < self.records.len())
                    .then(|| {
                        continuation
                            .offset
                            .map(|offset| offset.saturating_add(count))
                    })
                    .flatten()
            },
            |offset| {
                Some(
                    offset
                        .saturating_sub(self.records.len())
                        .saturating_add(count),
                )
            },
        );
        let mut parts = Vec::new();
        if let Some(next) = next {
            parts.push(match continuation.revision {
                Some(revision) => format!(
                    "more results; pass \"offset\": {next}, \"revision\": {revision} for the next page"
                ),
                None => format!("more results; pass \"offset\": {next} for the next page"),
            });
        } else if continuation.scan_notice.is_none() {
            parts.push("more results omitted".into());
        }
        if let Some(notice) = &continuation.scan_notice {
            parts.push(notice.clone());
        }
        Some(parts.join("\n"))
    }
}

pub(crate) fn present_unstructured(
    name: &str,
    output: &str,
    budget: PresentationBudget,
) -> ToolPresentation {
    if output.len() <= budget.max_bytes {
        return ToolPresentation::complete(output);
    }
    if budget.max_bytes == 0 {
        return ToolPresentation::omitted(
            "[tool result omitted by aggregate presentation budget; raw result retained separately]",
        );
    }
    if name == "shell" {
        let head_target = budget.max_bytes / 2 + budget.max_bytes % 2;
        let tail_target = budget.max_bytes.saturating_sub(head_target);
        let mut head_end = head_target.min(output.len());
        while head_end > 0 && !output.is_char_boundary(head_end) {
            head_end -= 1;
        }
        let mut tail_start = output.len().saturating_sub(tail_target);
        while tail_start < output.len() && !output.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        let tail_bytes = output.len().saturating_sub(tail_start);
        let discarded_bytes = output
            .len()
            .saturating_sub(head_end.saturating_add(tail_bytes));
        let text = format!(
            "{}\n[truncated {discarded_bytes} bytes by result limit; model sees first {head_end} and last {tail_bytes} bytes]\n{}",
            &output[..head_end],
            &output[tail_start..]
        );
        return ToolPresentation::bounded(text, 0, false);
    }
    let marker = "\n[tool result truncated by aggregate presentation budget; omitted content is not a complete record]";
    let mut end = budget
        .max_bytes
        .saturating_sub(marker.len())
        .min(output.len());
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    ToolPresentation::bounded(format!("{}{marker}", &output[..end]), 0, false)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DependencyKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FastStamp {
    pub(crate) kind: DependencyKind,
    pub(crate) len: u64,
    pub(crate) modified_nanos: Option<u128>,
    pub(crate) file_id: Option<String>,
    pub(crate) operation_digest: Option<String>,
}

impl FastStamp {
    pub(crate) fn from_file(
        kind: DependencyKind,
        file: &File,
        metadata: &Metadata,
        operation_digest: Option<String>,
    ) -> Self {
        Self {
            kind,
            len: metadata.len(),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
            file_id: file_identity(file, metadata),
            operation_digest,
        }
    }

    pub(crate) fn observed_directory(entry_count: usize, digest: String) -> Self {
        Self {
            kind: DependencyKind::Directory,
            len: u64::try_from(entry_count).unwrap_or(u64::MAX),
            modified_nanos: None,
            file_id: None,
            operation_digest: Some(digest),
        }
    }

    pub(crate) fn digest(&self) -> String {
        hash_fields(&[
            b"slim-fast-stamp-v1",
            format!("{:?}", self.kind).as_bytes(),
            self.len.to_string().as_bytes(),
            self.modified_nanos
                .unwrap_or_default()
                .to_string()
                .as_bytes(),
            self.file_id.as_deref().unwrap_or("").as_bytes(),
            self.operation_digest.as_deref().unwrap_or("").as_bytes(),
        ])
    }

    pub(crate) fn comparison_digest(&self) -> Option<String> {
        if self.kind == DependencyKind::Directory {
            return Some(self.digest());
        }
        if self.modified_nanos.is_none() && self.file_id.is_none() {
            return None;
        }
        Some(hash_fields(&[
            b"slim-fast-file-identity-v1",
            self.len.to_string().as_bytes(),
            self.modified_nanos
                .unwrap_or_default()
                .to_string()
                .as_bytes(),
            self.file_id.as_deref().unwrap_or("").as_bytes(),
        ]))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DependencyObservation {
    pub(crate) path: PathBuf,
    pub(crate) stamp: FastStamp,
}

impl DependencyObservation {
    pub(crate) fn key(&self) -> String {
        let kind = match self.stamp.kind {
            DependencyKind::File => "file",
            DependencyKind::Directory => "directory",
        };
        format!("{kind}:{}", path_identity(&self.path))
    }

    pub(crate) fn stamp_matches(&self) -> bool {
        if self.stamp.kind == DependencyKind::File {
            let Ok(file) = File::open(&self.path) else {
                return false;
            };
            if verify_opened_path(&file, &self.path).is_err() {
                return false;
            }
            let Ok(metadata) = file.metadata() else {
                return false;
            };
            let current = FastStamp::from_file(DependencyKind::File, &file, &metadata, None);
            return self.stamp.file_id.is_some()
                && self.stamp.modified_nanos.is_some()
                && current.file_id == self.stamp.file_id
                && current.len == self.stamp.len
                && current.modified_nanos == self.stamp.modified_nanos;
        }
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return false;
        };
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos());
        modified_nanos == self.stamp.modified_nanos && self.stamp.modified_nanos.is_some()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MutationObservation {
    pub(crate) path: PathBuf,
    pub(crate) before_content_digest: Option<String>,
    pub(crate) after: FastStamp,
}

impl MutationObservation {
    pub(crate) fn changed(&self) -> bool {
        self.before_content_digest.as_deref() != self.after.operation_digest.as_deref()
    }
}

#[derive(Clone, Debug)]
pub(crate) enum PreparedToolArguments {
    Read {
        offset: usize,
        /// `None` when the model omitted the limit: the default first page may
        /// widen to a full-file read. `Some` is always honored literally.
        max_lines: Option<usize>,
    },
    List {
        offset: usize,
        max_entries: usize,
        cursor: Option<String>,
    },
    Search {
        patterns: Vec<String>,
        context_lines: usize,
        offset: usize,
        max_hits: usize,
        cursor: Option<String>,
    },
    Write {
        content: String,
        expected: Option<String>,
    },
    Patch {
        edits: Vec<(String, String)>,
    },
    Shell {
        command: String,
        args: Option<Vec<String>>,
        timeout_ms: u64,
    },
    CodeIntel(CodeIntelRequest),
    External,
}

impl PreparedToolArguments {
    pub(crate) fn is_diagnostics(&self) -> bool {
        matches!(self, Self::CodeIntel(CodeIntelRequest::Diagnostics(_)))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedToolInvocation {
    pub(crate) mode: OperatingMode,
    pub(crate) name: String,
    pub(crate) arguments: PreparedToolArguments,
    /// Short, non-sensitive notes about compatibility transforms applied while
    /// admitting the call (for example an alias or a bounded context value).
    pub(crate) admission_notes: Vec<String>,
    pub(crate) canonical_workspace: PathBuf,
    pub(crate) target_paths: Vec<PathBuf>,
    pub(crate) spec: Option<ToolOperationalSpec>,
    pub(crate) canonical_fingerprint: String,
    pub(crate) preparation_us: u64,
    pub(crate) error: Option<String>,
    /// The call was rejected by deterministic argument admission before any
    /// executor could run. Workspace/path resolution failures intentionally do
    /// not set this bit because their effects remain unclassified.
    pub(crate) structural_rejection: bool,
}

impl PreparedToolInvocation {
    pub(crate) fn reusable_evidence(&self) -> bool {
        self.error.is_none()
            && self.spec.is_some_and(|spec| {
                spec.cacheability == ToolCacheability::Evidence
                    && spec.replay_policy == ToolReplayPolicy::EquivalentEvidence
                    && spec.effect_class == ToolEffectClass::SnapshotRead
            })
    }

    pub(crate) fn new(
        mode: OperatingMode,
        cwd: &Path,
        name: &str,
        raw_arguments: &str,
        native_spec: Option<ToolOperationalSpec>,
    ) -> Self {
        let started = Instant::now();
        let workspace = canonical_workspace(cwd);
        let workspace_preparation_us = elapsed_us(started);
        Self::from_workspace(
            mode,
            cwd,
            &workspace,
            workspace_preparation_us,
            name,
            raw_arguments,
            native_spec,
        )
    }

    pub(crate) fn from_workspace(
        mode: OperatingMode,
        cwd: &Path,
        workspace: &Result<PathBuf, String>,
        workspace_preparation_us: u64,
        name: &str,
        raw_arguments: &str,
        native_spec: Option<ToolOperationalSpec>,
    ) -> Self {
        let started = Instant::now();
        let (canonical_workspace, workspace_error) = match workspace {
            Ok(workspace) => (workspace.clone(), None),
            Err(message) => (absolute_fallback(cwd), Some(message.clone())),
        };
        let (mut arguments, mut error) = match serde_json::from_str::<Value>(raw_arguments) {
            Ok(arguments) => (arguments, None),
            Err(error) => (
                Value::Null,
                Some(format!("invalid tool arguments: {error}")),
            ),
        };
        let mut structural_rejection = error.is_some();
        if error.is_none() {
            error = workspace_error;
        }
        let mut target_paths = Vec::new();
        let mut admission_notes = Vec::new();
        if error.is_none() {
            if let Err(message) = materialize_defaults_and_paths(
                name,
                &canonical_workspace,
                &mut arguments,
                &mut target_paths,
                &mut admission_notes,
            ) {
                error = Some(message);
                structural_rejection =
                    !is_unclassifiable_preparation_error(error.as_deref().unwrap_or_default());
            }
        }
        let typed_arguments = if error.is_none() {
            match typed_arguments(
                name,
                &mut arguments,
                &canonical_workspace,
                &target_paths,
                &mut admission_notes,
            ) {
                Ok(arguments) => arguments,
                Err(message) => {
                    error = Some(message);
                    structural_rejection = true;
                    PreparedToolArguments::External
                }
            }
        } else {
            PreparedToolArguments::External
        };
        // Identity and operational classification are derived only after the
        // admission/typing pass.  This keeps clamped, filtered and aliased
        // values aligned across execution, scheduling and fingerprints.
        let effective_arguments = canonical_json(&arguments);
        let spec = effective_spec(native_spec, name, &arguments);
        let canonical_fingerprint = spec.map_or_else(
            || {
                hash_fields(&[
                    b"slim-prepared-call-v1",
                    name.as_bytes(),
                    effective_arguments.as_bytes(),
                ])
            },
            |spec| {
                hash_fields(&[
                    b"slim-prepared-call-v1",
                    name.as_bytes(),
                    effective_arguments.as_bytes(),
                    format!("{:?}", spec.effect_class).as_bytes(),
                    format!("{:?}", spec.cacheability).as_bytes(),
                    format!("{:?}", spec.volatility).as_bytes(),
                    format!("{:?}", spec.dependency_scope).as_bytes(),
                    format!("{:?}", spec.replay_policy).as_bytes(),
                ])
            },
        );
        // Only calls with a known operational contract can be classified as a
        // deterministic rejection. Unknown/external envelopes retain the
        // existing unclassifiable path even when their JSON is malformed.
        structural_rejection &= spec.is_some();
        Self {
            mode,
            name: name.to_owned(),
            arguments: typed_arguments,
            admission_notes,
            canonical_workspace,
            target_paths,
            spec,
            canonical_fingerprint,
            preparation_us: workspace_preparation_us.saturating_add(elapsed_us(started)),
            error,
            structural_rejection,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ToolExecutionReceipt {
    pub(crate) effects_uncertain: bool,
    pub(crate) dependencies: Vec<DependencyObservation>,
    pub(crate) mutations: Vec<MutationObservation>,
    pub(crate) modified_paths: Vec<PathBuf>,
    pub(crate) revision_before: u64,
    pub(crate) revision_after: u64,
    pub(crate) bytes_read: u64,
    pub(crate) preparation_us: u64,
    pub(crate) execution_us: u64,
    pub(crate) finalization_us: u64,
    pub(crate) synced_text: Option<crate::codeintel::CodeIntelFileUpdate>,
    pub(crate) process: Option<crate::process::ProcessExecutionFacts>,
    pub(crate) presentation: Option<ToolPresentationSource>,
}

impl ToolExecutionReceipt {
    pub(crate) fn unobserved(
        prepared: &PreparedToolInvocation,
        revision_before: u64,
        revision_after: u64,
        execution_us: u64,
    ) -> Self {
        Self {
            dependencies: Vec::new(),
            mutations: Vec::new(),
            modified_paths: Vec::new(),
            effects_uncertain: false,
            revision_before,
            revision_after,
            bytes_read: 0,
            preparation_us: prepared.preparation_us,
            execution_us,
            finalization_us: 0,
            synced_text: None,
            process: None,
            presentation: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ToolExecutionError {
    pub(crate) effects_uncertain: bool,
    pub(crate) error: ToolError,
    pub(crate) context: Option<String>,
    pub(crate) dependencies: Vec<DependencyObservation>,
    pub(crate) mutations: Vec<MutationObservation>,
    pub(crate) bytes_read: u64,
}

impl ToolExecutionError {
    pub(crate) fn observed(
        error: ToolError,
        dependencies: Vec<DependencyObservation>,
        bytes_read: u64,
    ) -> Self {
        Self {
            error,
            effects_uncertain: false,
            context: None,
            dependencies,
            mutations: Vec::new(),
            bytes_read,
        }
    }
}

impl From<ToolError> for ToolExecutionError {
    fn from(error: ToolError) -> Self {
        Self::observed(error, Vec::new(), 0)
    }
}

impl From<std::io::Error> for ToolExecutionError {
    fn from(error: std::io::Error) -> Self {
        ToolError::from(error).into()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ToolExecutionOutcome {
    pub(crate) result: ToolResult,
    pub(crate) receipt: ToolExecutionReceipt,
}

pub(crate) fn digest_bytes(domain: &[u8], bytes: &[u8]) -> String {
    hash_fields(&[domain, bytes])
}

pub(crate) fn hash_fields(fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hash_field(&mut hasher, field);
    }
    hex_digest(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(field);
}

pub(crate) fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

pub(crate) fn canonical_workspace(cwd: &Path) -> Result<PathBuf, String> {
    cwd.canonicalize()
        .map_err(|error| format!("workspace root cannot be resolved: {error}"))
}

fn absolute_fallback(cwd: &Path) -> PathBuf {
    if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| cwd.to_path_buf(), |root| root.join(cwd))
    }
}

fn is_unclassifiable_preparation_error(message: &str) -> bool {
    message.starts_with("workspace root cannot be resolved:")
        || message.starts_with("path cannot be resolved:")
        || message == "path escapes the workspace"
}

fn materialize_defaults_and_paths(
    tool_name: &str,
    cwd: &Path,
    arguments: &mut Value,
    target_paths: &mut Vec<PathBuf>,
    admission_notes: &mut Vec<String>,
) -> Result<(), String> {
    let object = arguments
        .as_object_mut()
        .ok_or_else(|| "tool arguments must be a JSON object".to_owned())?;
    reject_unknown_native_fields(tool_name, object)?;
    match tool_name {
        "read" => {
            insert_default(object, "offset", Value::from(1));
            normalize_read_lines_alias(object, admission_notes)?;
            // `max_lines` is deliberately not materialized: an absent limit and
            // an explicit DEFAULT_MAX_READ_LINES request are different calls.
        }
        "list" => {
            insert_default_path(object, ".")?;
            insert_default(object, "offset", Value::from(1));
            insert_default(
                object,
                "max_entries",
                Value::from(u64::try_from(DEFAULT_MAX_ENTRIES).unwrap_or(u64::MAX)),
            );
            normalize_cursor(object, "list", admission_notes)?;
        }
        "search" => {
            insert_default(object, "context_lines", Value::from(0));
            insert_default_path(object, ".")?;
            insert_default(object, "offset", Value::from(1));
            insert_default(
                object,
                "max_hits",
                Value::from(u64::try_from(DEFAULT_MAX_HITS).unwrap_or(u64::MAX)),
            );
            // A present field with the wrong type is rejected rather than
            // silently treated as omitted; so is the ambiguous pair.
            if object
                .get("query")
                .is_some_and(|value| !value.is_null() && value.as_str().is_none())
            {
                return Err("search query must be a string".into());
            }
            if object
                .get("patterns")
                .is_some_and(|value| !value.is_null() && value.as_array().is_none())
            {
                return Err("search patterns must be an array of strings".into());
            }
            match (has_search_query(object), has_search_patterns(object)) {
                (false, false) => {
                    return Err("search requires exactly one of query or patterns".into());
                }
                (true, true) => {
                    let Some(query) = object["query"].as_str().map(str::to_owned) else {
                        return Err("search requires exactly one of query or patterns".into());
                    };
                    // Collapse only a literal duplicate spelling of the same
                    // term; any other combination would silently drop terms.
                    let duplicate = object["patterns"].as_array().is_some_and(|patterns| {
                        !patterns.is_empty()
                            && patterns
                                .iter()
                                .all(|value| value.as_str() == Some(query.as_str()))
                    });
                    if !duplicate {
                        return Err("search requires exactly one of query or patterns".into());
                    }
                    object.remove("query");
                    object.remove("patterns");
                    object.insert("patterns".into(), Value::Array(vec![Value::from(query)]));
                }
                (true, false) => {
                    let Some(query) = object.remove("query") else {
                        return Err("search requires exactly one of query or patterns".into());
                    };
                    object.remove("patterns");
                    object.insert("patterns".into(), Value::Array(vec![query]));
                }
                (false, true) => {
                    object.remove("query");
                }
            }
            normalize_cursor(object, "search", admission_notes)?;
        }
        "code_intel" => {
            insert_default(object, "include_info", Value::Bool(false));
            insert_default(
                object,
                "max_results",
                Value::from(u64::try_from(DEFAULT_CODE_INTEL_LIMIT).unwrap_or(u64::MAX)),
            );
        }
        "shell" => {
            insert_default(object, "timeout_ms", Value::from(30_000));
            if object.get("args").is_some_and(Value::is_null) {
                object.remove("args");
            }
        }
        "write" if object.get("expected").is_some_and(Value::is_null) => {
            // `null` is the documented create/observed-precondition spelling;
            // it has the same effective meaning as an omitted expected value.
            object.remove("expected");
        }
        _ => {}
    }
    let required_path = matches!(tool_name, "read" | "list" | "search" | "write" | "patch");
    let optional_code_intel_path = tool_name == "code_intel"
        && object
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| !path.is_empty());
    if required_path || optional_code_intel_path {
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing required string field `path`".to_owned())?;
        if path.is_empty() {
            return Err("path must not be empty".to_owned());
        }
        let normalized = resolve_workspace_path_from_root(cwd, path)?;
        target_paths.push(normalized.clone());
        object.insert("path".into(), Value::String(path_identity(&normalized)));
    }
    Ok(())
}

fn native_argument_keys(tool_name: &str) -> Option<&'static [&'static str]> {
    match tool_name {
        "read" => Some(&["path", "offset", "max_lines", "lines"]),
        "list" => Some(&["path", "offset", "max_entries", "cursor"]),
        "search" => Some(&[
            "path",
            "query",
            "patterns",
            "context_lines",
            "max_hits",
            "offset",
            "cursor",
        ]),
        "write" => Some(&["path", "content", "expected"]),
        "patch" => Some(&["path", "edits", "expected", "replacement"]),
        "shell" => Some(&["command", "args", "timeout_ms"]),
        "code_intel" => Some(&[
            "action",
            "path",
            "line",
            "column",
            "symbol",
            "query",
            "include_info",
            "max_results",
            "offset",
            "revision",
        ]),
        _ => None,
    }
}

fn reject_unknown_native_fields(
    tool_name: &str,
    object: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    let Some(allowed) = native_argument_keys(tool_name) else {
        // MCP/capability payloads are owned by their external provider.  Do
        // not impose a native-tool whitelist on those envelopes.
        return Ok(());
    };
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("{tool_name} has unknown argument `{key}`"));
    }
    if tool_name == "patch" {
        if let Some(edits) = object.get("edits").and_then(Value::as_array) {
            for (index, edit) in edits.iter().enumerate() {
                let Some(edit) = edit.as_object() else {
                    continue;
                };
                if let Some(key) = edit
                    .keys()
                    .find(|key| !matches!(key.as_str(), "expected" | "replacement"))
                {
                    return Err(format!("patch edit {index} has unknown argument `{key}`"));
                }
            }
        }
    }
    Ok(())
}

fn normalize_read_lines_alias(
    object: &mut serde_json::Map<String, Value>,
    admission_notes: &mut Vec<String>,
) -> Result<(), String> {
    let Some(lines_value) = object.get("lines").cloned() else {
        return Ok(());
    };
    let lines = integer_value(Some(&lines_value), "lines")?;
    if !(1..=MAX_READ_LINES_CAP).contains(&lines) {
        return Err(format!("read lines must be 1..={MAX_READ_LINES_CAP}"));
    }
    if let Some(max_lines_value) = object.get("max_lines") {
        let max_lines = integer_value(Some(max_lines_value), "max_lines")?;
        if !(1..=MAX_READ_LINES_CAP).contains(&max_lines) {
            return Err(format!("read max_lines must be 1..={MAX_READ_LINES_CAP}"));
        }
        if max_lines != lines {
            return Err("read lines conflicts with max_lines".into());
        }
    } else {
        object.insert("max_lines".into(), Value::from(lines));
    }
    object.remove("lines");
    admission_notes.push(format!("lines -> max_lines; limit {lines}"));
    Ok(())
}

fn normalize_cursor(
    object: &mut serde_json::Map<String, Value>,
    tool_name: &str,
    admission_notes: &mut Vec<String>,
) -> Result<(), String> {
    let Some(cursor_value) = object.get("cursor").cloned() else {
        return Ok(());
    };
    let cursor = cursor_value
        .as_str()
        .ok_or_else(|| "argument `cursor` must be a string".to_owned())?;
    let trimmed = cursor.trim();
    if trimmed.is_empty() {
        object.remove("cursor");
        admission_notes.push(format!("{tool_name} cursor blank omitted"));
    } else if trimmed != cursor {
        object.insert("cursor".into(), Value::String(trimmed.to_owned()));
        admission_notes.push(format!("{tool_name} cursor trimmed"));
    }
    Ok(())
}

fn canonicalize_code_intel_arguments(
    arguments: &mut Value,
    request: &CodeIntelRequest,
    admission_notes: &mut Vec<String>,
) {
    let Some(object) = arguments.as_object_mut() else {
        return;
    };
    // Existing parsing treats an empty query/path as absent.  Remove those
    // spellings before canonicalization so they cannot split identities.
    if object
        .get("query")
        .and_then(Value::as_str)
        .is_some_and(|query| query.trim().is_empty())
    {
        object.remove("query");
        admission_notes.push("code_intel query blank omitted".into());
    }
    if object
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|path| path.is_empty())
    {
        object.remove("path");
        admission_notes.push("code_intel path blank omitted".into());
    }
    if object.get("path").is_some_and(Value::is_null) {
        object.remove("path");
        admission_notes.push("code_intel path null omitted".into());
    }
    if object.get("revision").is_some_and(Value::is_null) {
        object.remove("revision");
    }

    const STATUS_KEYS: &[&str] = &["action"];
    const POSITION_KEYS: &[&str] = &[
        "action",
        "path",
        "line",
        "column",
        "symbol",
        "max_results",
        "offset",
        "revision",
    ];
    const SYMBOL_KEYS: &[&str] = &[
        "action",
        "path",
        "query",
        "max_results",
        "offset",
        "revision",
    ];
    const DIAGNOSTIC_KEYS: &[&str] = &["action", "path", "include_info", "max_results"];
    let (allowed, effective_limit, offset, revision) = match request {
        CodeIntelRequest::Status { .. } => (STATUS_KEYS, None, None, None),
        CodeIntelRequest::Definition(query)
        | CodeIntelRequest::References(query)
        | CodeIntelRequest::Hover(query) => (
            POSITION_KEYS,
            Some(query.max_results),
            Some(query.offset),
            query.revision,
        ),
        CodeIntelRequest::Symbols(query) => (
            SYMBOL_KEYS,
            Some(query.max_results),
            Some(query.offset),
            query.revision,
        ),
        CodeIntelRequest::Diagnostics(query) => {
            (DIAGNOSTIC_KEYS, Some(query.max_results), None, None)
        }
    };
    if let Some(effective_limit) = effective_limit {
        let raw_limit = object.get("max_results").and_then(Value::as_u64);
        match raw_limit {
            None => admission_notes.push(format!(
                "code_intel max_results defaulted to {effective_limit}"
            )),
            Some(raw) => {
                let raw_usize = usize::try_from(raw).unwrap_or(usize::MAX);
                if raw_usize != effective_limit {
                    admission_notes.push(format!(
                        "code_intel max_results {raw_usize} -> {effective_limit}"
                    ));
                }
            }
        }
        object.insert("max_results".into(), Value::from(effective_limit));
    }
    if let Some(offset) = offset {
        object.insert("offset".into(), Value::from(offset));
    } else {
        object.remove("offset");
    }
    match revision {
        Some(revision) => {
            object.insert("revision".into(), Value::from(revision));
        }
        None => {
            object.remove("revision");
        }
    }
    if let CodeIntelRequest::Definition(query)
    | CodeIntelRequest::References(query)
    | CodeIntelRequest::Hover(query) = request
    {
        object.insert("line".into(), Value::from(query.line));
        object.insert("column".into(), Value::from(query.column));
        match query.symbol.as_deref() {
            Some(symbol) => {
                object.insert("symbol".into(), Value::String(symbol.to_owned()));
            }
            None => {
                object.remove("symbol");
            }
        }
    }
    if let CodeIntelRequest::Diagnostics(query) = request {
        object.insert("include_info".into(), Value::Bool(query.include_info));
    }
    object.retain(|key, _| allowed.contains(&key.as_str()));
}

fn typed_arguments(
    tool_name: &str,
    arguments: &mut Value,
    canonical_workspace: &Path,
    target_paths: &[PathBuf],
    admission_notes: &mut Vec<String>,
) -> Result<PreparedToolArguments, String> {
    let string_argument = |name: &str| {
        arguments
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("missing string argument: {name}"))
    };
    let nonempty_string_argument = |name: &str| {
        string_argument(name).and_then(|value| {
            if value.is_empty() {
                Err(format!("missing string argument: {name}"))
            } else {
                Ok(value)
            }
        })
    };
    match tool_name {
        "read" => {
            let max_lines = if arguments.get("max_lines").is_some() {
                Some(integer_argument(arguments, "max_lines")?)
            } else {
                None
            };
            let offset = integer_argument(arguments, "offset")?;
            if max_lines.is_some_and(|max_lines| max_lines == 0 || max_lines > MAX_READ_LINES_CAP) {
                return Err(format!("read max_lines must be 1..={MAX_READ_LINES_CAP}"));
            }
            if offset == 0 {
                return Err("read offset must be at least 1".into());
            }
            Ok(PreparedToolArguments::Read { offset, max_lines })
        }
        "list" => {
            let offset = integer_argument(arguments, "offset")?;
            let max_entries = integer_argument(arguments, "max_entries")?;
            if offset == 0 {
                return Err("list offset must be at least 1".into());
            }
            if max_entries == 0 || max_entries > MAX_ENTRIES_CAP {
                return Err(format!("list max_entries must be 1..={MAX_ENTRIES_CAP}"));
            }
            Ok(PreparedToolArguments::List {
                offset,
                max_entries,
                cursor: optional_nonempty_string_argument(arguments, "cursor")?,
            })
        }
        "search" => {
            let patterns = arguments
                .get("patterns")
                .and_then(Value::as_array)
                .ok_or_else(|| "search patterns must be an array of strings".to_owned())?
                .iter()
                .map(|pattern| {
                    pattern
                        .as_str()
                        .filter(|pattern| !pattern.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| "search patterns must contain non-empty strings".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if patterns.is_empty() || patterns.len() > MAX_SEARCH_PATTERNS {
                return Err(format!(
                    "search patterns must contain 1..={MAX_SEARCH_PATTERNS} strings"
                ));
            }
            let offset = integer_argument(arguments, "offset")?;
            let max_hits = integer_argument(arguments, "max_hits")?;
            if offset == 0 {
                return Err("search offset must be at least 1".into());
            }
            if max_hits == 0 || max_hits > MAX_HITS_CAP {
                return Err(format!("search max_hits must be 1..={MAX_HITS_CAP}"));
            }
            let requested_context_lines = integer_argument(arguments, "context_lines")?;
            let context_lines = requested_context_lines.min(super::search::MAX_CONTEXT_LINES);
            if context_lines != requested_context_lines {
                admission_notes.push(format!(
                    "context_lines {requested_context_lines} -> {context_lines}; maximum"
                ));
                arguments["context_lines"] = Value::from(context_lines);
            }
            Ok(PreparedToolArguments::Search {
                context_lines,
                patterns,
                offset,
                max_hits,
                cursor: optional_nonempty_string_argument(arguments, "cursor")?,
            })
        }
        "write" => {
            let expected = if arguments.get("expected").is_some_and(Value::is_null) {
                None
            } else {
                optional_string_argument(arguments, "expected")?
            };
            if expected
                .as_ref()
                .is_some_and(|text| text.len() > MAX_MUTATING_FILE_BYTES)
            {
                return Err(format!(
                    "write expected exceeds the {MAX_MUTATING_FILE_BYTES}-byte mutation safety limit; use patch for a local edit, or write after a complete read without expected"
                ));
            }
            Ok(PreparedToolArguments::Write {
                content: string_argument("content")?,
                expected,
            })
        }
        "patch" => {
            let edits = if let Some(edits) = arguments.get("edits") {
                if arguments.get("expected").is_some() || arguments.get("replacement").is_some() {
                    return Err("patch accepts edits or expected/replacement, not both".into());
                }
                let edits = edits.as_array().ok_or("patch edits must be an array")?;
                if edits.is_empty() || edits.len() > super::patch::MAX_PATCH_EDITS {
                    return Err(format!(
                        "patch edits must contain 1..={} entries",
                        super::patch::MAX_PATCH_EDITS
                    ));
                }
                edits
                    .iter()
                    .map(|edit| {
                        let expected = edit
                            .get("expected")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                            .ok_or("each edit needs a nonempty expected string")?;
                        let replacement = edit
                            .get("replacement")
                            .and_then(Value::as_str)
                            .ok_or("each edit needs a replacement string")?;
                        Ok((expected.to_owned(), replacement.to_owned()))
                    })
                    .collect::<Result<Vec<_>, String>>()?
            } else {
                vec![(
                    nonempty_string_argument("expected")?,
                    string_argument("replacement")?,
                )]
            };
            // Legacy `expected`/`replacement` and the batch form have the same
            // effective operation.  Store one canonical shape for identity,
            // while continuing to accept the legacy spelling at the boundary.
            let canonical_edits = edits
                .iter()
                .map(|(expected, replacement)| {
                    let mut edit = serde_json::Map::new();
                    edit.insert("expected".into(), Value::String(expected.clone()));
                    edit.insert("replacement".into(), Value::String(replacement.clone()));
                    Value::Object(edit)
                })
                .collect();
            if let Some(object) = arguments.as_object_mut() {
                let legacy = object.get("edits").is_none();
                object.remove("expected");
                object.remove("replacement");
                object.insert("edits".into(), Value::Array(canonical_edits));
                if legacy {
                    admission_notes.push("patch legacy fields normalized to edits".into());
                }
            }
            Ok(PreparedToolArguments::Patch { edits })
        }
        "shell" => {
            let timeout_ms = u64_argument(arguments, "timeout_ms")?;
            if !(1..=120_000).contains(&timeout_ms) {
                return Err("shell timeout_ms must be 1..=120000".into());
            }
            let command = nonempty_string_argument("command")?;
            let args = arguments
                .get("args")
                .map(|value| {
                    value
                        .as_array()
                        .ok_or("shell args must be an array of strings")?
                        .iter()
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::to_owned)
                                .ok_or_else(|| "shell args must contain only strings".to_owned())
                        })
                        .collect::<Result<Vec<_>, String>>()
                })
                .transpose()?;
            admit_shell_command(&command, args.is_some(), admission_notes)?;
            Ok(PreparedToolArguments::Shell {
                command,
                args,
                timeout_ms,
            })
        }
        "code_intel" => {
            let request = parse_prepared_code_intel_request(
                canonical_workspace,
                arguments,
                target_paths.first().map(PathBuf::as_path),
            )?;
            canonicalize_code_intel_arguments(arguments, &request, admission_notes);
            Ok(PreparedToolArguments::CodeIntel(request))
        }
        _ => Ok(PreparedToolArguments::External),
    }
}

/// Shell command admission for common model slips. A serialized tool-call
/// payload in `command` is rejected outright; possible bash/PowerShell
/// compatibility issues in the script form, a multi-word `command` with
/// `args` supplied, and possible inline `-c`/`-e` quoting risks are reported as
/// admission notes attached to the call's output.
fn admit_shell_command(
    command: &str,
    has_args: bool,
    admission_notes: &mut Vec<String>,
) -> Result<(), String> {
    let trimmed = command.trim_start();
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(trimmed) {
            let tool_payload = ["command", "args", "timeout_ms", "path", "content", "edits"]
                .iter()
                .any(|key| payload.contains_key(*key));
            if tool_payload {
                return Err(
                    "shell command must be a command line, not a JSON tool payload; pass the command text in `command`"
                        .into(),
                );
            }
        }
    }
    if has_args {
        let mut words = command.split_whitespace();
        let first = words.next().unwrap_or("");
        let bare_name = !first.contains(['\\', '/', '"', '\'']);
        if bare_name && words.next().is_some() {
            admission_notes.push(
                "with `args` supplied, `command` is the executable name alone; for a full command line omit `args`"
                    .into(),
            );
        }
        return Ok(());
    }
    const BASH_ONLY: &[&str] = &[
        "<<",
        "head -",
        "tail -",
        "ls -",
        "export ",
        "which ",
        "chmod ",
        "sed -i",
        "awk ",
        "xargs",
        "/dev/null",
        "source ",
    ];
    let found = BASH_ONLY
        .iter()
        .filter(|token| command.contains(**token))
        .take(2)
        .map(|token| token.trim_end().to_owned())
        .collect::<Vec<_>>();
    if !found.is_empty() {
        admission_notes.push(format!(
            "possible bash syntax in PowerShell: `{}` may parse differently; verify compatibility/quoting",
            found.join("`, `")
        ));
    }
    if command.contains('\n') && has_inline_eval(command) {
        admission_notes.push(
            "possible quoting risk: this multiline script uses inline `-c`/`-e` eval; if parsing fails, consider a script file"
                .into(),
        );
    }
    Ok(())
}

/// `python -c "..."`-style inline eval can carry a quoting risk when the
/// generated argument contains raw newlines. This intentionally remains a
/// lightweight heuristic rather than trying to parse either shell language.
fn has_inline_eval(command: &str) -> bool {
    const EVAL_FORMS: &[&str] = &[
        "python -c",
        "python3 -c",
        "py -c",
        "node -e",
        "deno eval",
        "perl -e",
        "ruby -e",
        "php -r",
    ];
    let command = command.to_ascii_lowercase();
    EVAL_FORMS.iter().any(|form| {
        command.match_indices(form).any(|(index, _)| {
            command[..index]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
        })
    })
}

fn integer_argument(arguments: &Value, name: &str) -> Result<usize, String> {
    integer_value(arguments.get(name), name)
}

fn integer_value(value: Option<&Value>, name: &str) -> Result<usize, String> {
    let value = value
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("argument `{name}` must be an unsigned integer"))?;
    usize::try_from(value).map_err(|_| format!("argument `{name}` is too large"))
}

fn u64_argument(arguments: &Value, name: &str) -> Result<u64, String> {
    arguments
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("argument `{name}` must be an unsigned integer"))
}

fn optional_string_argument(arguments: &Value, name: &str) -> Result<Option<String>, String> {
    arguments
        .get(name)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("argument `{name}` must be a string"))
        })
        .transpose()
}

fn optional_nonempty_string_argument(
    arguments: &Value,
    name: &str,
) -> Result<Option<String>, String> {
    Ok(
        optional_string_argument(arguments, name)?.and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then_some(trimmed.to_owned())
        }),
    )
}

fn insert_default(object: &mut serde_json::Map<String, Value>, key: &str, value: Value) {
    if !object.contains_key(key) {
        object.insert(key.into(), value);
    }
}

fn insert_default_path(
    object: &mut serde_json::Map<String, Value>,
    default: &str,
) -> Result<(), String> {
    match object.get("path") {
        // Present with an incompatible type: reject instead of widening the
        // operation to the workspace root.
        Some(value) if !value.is_null() && value.as_str().is_none() => {
            Err("argument `path` must be a string".into())
        }
        Some(value) if value.as_str().is_some_and(|path| !path.trim().is_empty()) => Ok(()),
        _ => {
            object.insert("path".into(), Value::from(default));
            Ok(())
        }
    }
}

fn has_search_query(object: &serde_json::Map<String, Value>) -> bool {
    object
        .get("query")
        .and_then(Value::as_str)
        .is_some_and(|query| !query.trim().is_empty())
}

fn has_search_patterns(object: &serde_json::Map<String, Value>) -> bool {
    object
        .get("patterns")
        .and_then(Value::as_array)
        .is_some_and(|patterns| {
            patterns
                .iter()
                .any(|pattern| pattern.as_str().is_some_and(|text| !text.trim().is_empty()))
        })
}

fn effective_spec(
    native_spec: Option<ToolOperationalSpec>,
    tool_name: &str,
    arguments: &Value,
) -> Option<ToolOperationalSpec> {
    if tool_name == "shell" && allowlisted_validation(arguments) {
        return Some(ToolOperationalSpec {
            effect_class: ToolEffectClass::Validation,
            cacheability: ToolCacheability::None,
            volatility: ToolVolatility::Volatile,
            dependency_scope: ToolDependencyScope::Unknown,
            replay_policy: ToolReplayPolicy::Never,
        });
    }
    native_spec.or(match tool_name {
        "ask_question" => Some(ToolOperationalSpec {
            effect_class: ToolEffectClass::Interaction,
            cacheability: ToolCacheability::None,
            volatility: ToolVolatility::ExternalInput,
            dependency_scope: ToolDependencyScope::Interaction,
            replay_policy: ToolReplayPolicy::Never,
        }),
        "todo" => Some(ToolOperationalSpec {
            effect_class: ToolEffectClass::InternalState,
            cacheability: ToolCacheability::None,
            volatility: ToolVolatility::Internal,
            dependency_scope: ToolDependencyScope::Internal,
            replay_policy: ToolReplayPolicy::Never,
        }),
        "skill" => Some(ToolOperationalSpec {
            effect_class: ToolEffectClass::PotentiallyVolatile,
            cacheability: ToolCacheability::None,
            volatility: ToolVolatility::Volatile,
            dependency_scope: ToolDependencyScope::Unknown,
            replay_policy: ToolReplayPolicy::Never,
        }),
        _ => None,
    })
}

fn allowlisted_validation(arguments: &Value) -> bool {
    let Some(command) = arguments.get("command").and_then(Value::as_str) else {
        return false;
    };
    let tokens = if let Some(args) = arguments.get("args").filter(|value| !value.is_null()) {
        let Some(args) = args.as_array() else {
            return false;
        };
        let mut tokens = vec![command];
        for argument in args {
            let Some(argument) = argument.as_str() else {
                return false;
            };
            tokens.push(argument);
        }
        tokens
    } else {
        if command.chars().any(|character| {
            matches!(
                character,
                ';' | '|' | '&' | '>' | '<' | '`' | '$' | '\n' | '\r'
            )
        }) {
            return false;
        }
        command.split_whitespace().collect::<Vec<_>>()
    };
    if tokens.iter().any(|token| {
        let token = token
            .strip_prefix("'")
            .and_then(|token| token.strip_suffix("'"))
            .or_else(|| {
                token
                    .strip_prefix("\"")
                    .and_then(|token| token.strip_suffix("\""))
            })
            .unwrap_or(token);
        matches!(token, "--fix" | "--help" | "-h" | "--version" | "-V")
    }) {
        return false;
    }
    let mut index = 0;
    if tokens.first().is_some_and(|token| *token == "env") {
        index = 1;
        while index < tokens.len() {
            if tokens[index] == "-u" {
                index = index.saturating_add(2);
            } else if tokens[index].starts_with("--unset=") {
                index = index.saturating_add(1);
            } else {
                break;
            }
        }
    }
    if !tokens
        .get(index)
        .is_some_and(|token| matches!(*token, "cargo" | "cargo.exe"))
    {
        return false;
    }
    match tokens.get(index + 1).copied() {
        Some("test" | "check" | "clippy") => true,
        Some("fmt") => tokens[index + 2..].contains(&"--check"),
        _ => false,
    }
}

fn canonical_json(value: &Value) -> String {
    fn write_value(value: &Value, output: &mut String) {
        match value {
            Value::Object(object) => {
                output.push('{');
                let mut keys = object.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    output.push_str(&serde_json::to_string(key).expect("JSON key serializes"));
                    output.push(':');
                    write_value(&object[key], output);
                }
                output.push('}');
            }
            Value::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    write_value(value, output);
                }
                output.push(']');
            }
            _ => output.push_str(&serde_json::to_string(value).expect("JSON value serializes")),
        }
    }

    let mut output = String::new();
    write_value(value, &mut output);
    output
}

pub(crate) fn path_identity(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(windows)]
pub(crate) fn verify_opened_path(file: &File, expected: &Path) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFinalPathNameByHandleW(
            file: *mut c_void,
            path: *mut u16,
            path_len: u32,
            flags: u32,
        ) -> u32;
    }

    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: the handle belongs to a live File and buffer is writable for its
        // advertised length. Windows returns the number of UTF-16 code units.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle().cast(),
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                0,
            )
        };
        if length == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let length = length as usize;
        if length < buffer.len() {
            buffer.truncate(length);
            break;
        }
        buffer.resize(length.saturating_add(1), 0);
    }
    let actual = std::ffi::OsString::from_wide(&buffer);
    let actual = actual.to_string_lossy();
    let expected = expected.to_string_lossy();
    if actual.eq_ignore_ascii_case(&expected) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "opened path no longer matches the validated workspace path",
        ))
    }
}

#[cfg(any(target_os = "android", target_os = "linux"))]
pub(crate) fn verify_opened_path(file: &File, expected: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let actual = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    if actual == expected {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "opened path no longer matches the validated workspace path",
        ))
    }
}

#[cfg(all(unix, not(any(target_os = "android", target_os = "linux"))))]
pub(crate) fn verify_opened_path(_file: &File, expected: &Path) -> std::io::Result<()> {
    if std::fs::canonicalize(expected)? == expected {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "opened path no longer matches the validated workspace path",
        ))
    }
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn verify_opened_path(_file: &File, _expected: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn file_identity(file: &File, _metadata: &Metadata) -> Option<String> {
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(
            file: *mut c_void,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    let mut information = MaybeUninit::<ByHandleFileInformation>::uninit();
    // SAFETY: the handle belongs to a live File and Windows initializes the
    // complete output structure only when the call reports success.
    let succeeded = unsafe {
        GetFileInformationByHandle(file.as_raw_handle().cast(), information.as_mut_ptr())
    };
    if succeeded == 0 {
        return None;
    }
    // SAFETY: a successful GetFileInformationByHandle call initialized it.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low);
    Some(format!("{}:{file_index}", information.volume_serial_number))
}

#[cfg(unix)]
fn file_identity(_file: &File, metadata: &Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(not(any(windows, unix)))]
fn file_identity(_file: &File, _metadata: &Metadata) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::code_intel;
    use serde_json::json;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn workspace(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("slim-admission-{label}-{stamp}"));
        fs::create_dir_all(&path).expect("workspace");
        path
    }

    fn prepare(root: &Path, name: &str, arguments: Value) -> PreparedToolInvocation {
        PreparedToolInvocation::new(
            OperatingMode::Auto,
            root,
            name,
            &arguments.to_string(),
            None,
        )
    }

    #[test]
    fn read_projection_preserves_crlf_and_reports_oversized_records() {
        let records = (0..5)
            .map(|index| format!("line-{index}{}\r\n", "x".repeat(24)))
            .collect::<Vec<_>>();
        let source = ToolPresentationSource::Read {
            prefix: String::new(),
            full: records.concat(),
            first: 7,
            records: records.clone(),
            next_offset: None,
        };
        let limited = source.present(PresentationBudget { max_bytes: 120 });
        assert!(limited.delivered_records > 0 && limited.delivered_records < records.len());
        assert!(limited.text.starts_with(&records[0]));
        assert!(limited.text.contains("\r\n"));
        assert!(limited.text.contains("\"offset\": 8"), "{}", limited.text);
        assert!(!limited.oversized_record);

        let oversized = ToolPresentationSource::Read {
            prefix: String::new(),
            full: "é".repeat(128),
            first: 42,
            records: vec!["é".repeat(128)],
            next_offset: None,
        }
        .present(PresentationBudget { max_bytes: 24 });
        assert_eq!(oversized.delivered_records, 0);
        assert!(oversized.oversized_record);
        assert!(oversized.text.contains("\"offset\": 42"));
        assert!(oversized.text.contains("request a narrower page"));
    }

    #[test]
    fn code_intel_projection_keeps_grouped_reference_cursor_and_revision() {
        let outcome = crate::codeintel::CodeIntelOutcome {
            meta: crate::codeintel::CodeIntelMeta {
                server: "fixture".into(),
                state: crate::codeintel::CodeIntelServerState::Ready,
                completeness: crate::codeintel::CodeIntelCompleteness::Complete,
                document_version: Some(4),
                stale: false,
                elapsed_ms: 2,
            },
            payload: json!({
                "offset": 4,
                "total": 7,
                "shown": 3,
                "has_more": true,
                "next_offset": 8,
                "revision": 19,
                "files": [
                    {"file":"src/a.rs", "count":2, "results":[
                        {"line":10,"column":2,"context":"first"},
                        {"line":20,"column":4,"context":"second"}
                    ]},
                    {"file":"src/b.rs", "count":1, "results":[
                        {"line":30,"column":6,"context":"third"}
                    ]}
                ],
                "symbol":"target"
            }),
        };
        let full = code_intel::render_code_intel("references", &outcome);
        let presentation =
            code_intel::presentation_for_code_intel("references", &outcome, full.clone());
        assert_eq!(presentation.full, full);
        assert_eq!(presentation.records.len(), 3);
        assert_eq!(
            presentation.header_for_count(2),
            "code_intel references: target - 2 of 7 across 2 file(s) | complete"
        );
        assert_eq!(
            presentation.continuation_for_count(1).as_deref(),
            Some("more results; pass \"offset\": 6, \"revision\": 19 for the next page")
        );
    }

    #[test]
    fn code_intel_final_symbol_page_keeps_validity_when_budget_omits_rows() {
        let outcome = crate::codeintel::CodeIntelOutcome {
            meta: crate::codeintel::CodeIntelMeta {
                server: "fixture".into(),
                state: crate::codeintel::CodeIntelServerState::Ready,
                completeness: crate::codeintel::CodeIntelCompleteness::Complete,
                document_version: Some(9),
                stale: false,
                elapsed_ms: 1,
            },
            payload: json!({
                "kind":"document",
                "offset":3,
                "revision":23,
                "has_more":false,
                "symbols":[
                    {"name":"alpha","kind":"function","file":"src/lib.rs","line":1,"column":1},
                    {"name":"beta","kind":"function","file":"src/lib.rs","line":2,"column":1}
                ]
            }),
        };
        let full = code_intel::render_code_intel("symbol", &outcome);
        let presentation = code_intel::presentation_for_code_intel("symbol", &outcome, full);
        let continuation = presentation
            .continuation
            .as_ref()
            .expect("offset/revision preserve page validity");
        assert_eq!(continuation.next_offset, None);
        assert_eq!(continuation.offset, Some(3));
        let source = ToolPresentationSource::CodeIntel {
            prefix: String::new(),
            presentation,
        };
        let limited = source.present(PresentationBudget { max_bytes: 96 });
        assert!(limited.delivered_records < 2, "{}", limited.text);
        assert!(limited.text.contains("document_version: 9"));
        assert!(limited.text.contains("offset\": 3"), "{}", limited.text);
        assert!(limited.text.contains("revision\": 23"), "{}", limited.text);

        let empty_outcome = crate::codeintel::CodeIntelOutcome {
            meta: crate::codeintel::CodeIntelMeta {
                server: "fixture".into(),
                state: crate::codeintel::CodeIntelServerState::Ready,
                completeness: crate::codeintel::CodeIntelCompleteness::Complete,
                document_version: Some(10),
                stale: false,
                elapsed_ms: 1,
            },
            payload: json!({"kind":"document", "offset":3, "revision":24, "has_more":false, "symbols":[]}),
        };
        let empty = code_intel::presentation_for_code_intel(
            "symbol",
            &empty_outcome,
            code_intel::render_code_intel("symbol", &empty_outcome),
        );
        let empty_source = ToolPresentationSource::CodeIntel {
            prefix: String::new(),
            presentation: empty,
        };
        let empty_limited = empty_source.present(PresentationBudget { max_bytes: 0 });
        assert_eq!(empty_limited.delivered_records, 0);
        assert!(empty_limited.text.contains("code_intel action=symbol"));
        assert!(empty_limited.text.contains("document_version: 10"));
    }

    #[test]
    fn read_lines_alias_is_bounded_conflict_checked_and_identity_stable() {
        let root = workspace("read-lines");
        fs::write(root.join("text.txt"), "one\ntwo\n").expect("fixture");

        let alias = prepare(&root, "read", json!({"path":"text.txt", "lines":2}));
        let canonical = prepare(&root, "read", json!({"path":"text.txt", "max_lines":2}));
        assert!(alias.error.is_none(), "{:?}", alias.error);
        assert_eq!(alias.canonical_fingerprint, canonical.canonical_fingerprint);
        assert!(alias
            .admission_notes
            .iter()
            .any(|note| note.contains("lines") && note.contains("max_lines")));

        let equal = prepare(
            &root,
            "read",
            json!({"path":"text.txt", "lines":2, "max_lines":2}),
        );
        assert_eq!(equal.canonical_fingerprint, canonical.canonical_fingerprint);
        for lines in [json!(0), json!(MAX_READ_LINES_CAP + 1), json!("2")] {
            let rejected = prepare(&root, "read", json!({"path":"text.txt", "lines":lines}));
            assert!(rejected.error.is_some(), "accepted invalid lines: {lines}");
        }
        let conflict = prepare(
            &root,
            "read",
            json!({"path":"text.txt", "lines":1, "max_lines":2}),
        );
        assert!(conflict.error.is_some());

        let omitted = prepare(&root, "read", json!({"path":"text.txt"}));
        let explicit = prepare(
            &root,
            "read",
            json!({"path":"text.txt", "max_lines":MAX_READ_LINES_CAP}),
        );
        assert_ne!(
            omitted.canonical_fingerprint,
            explicit.canonical_fingerprint
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn native_unknown_fields_are_rejected_without_restricting_mcp_payloads() {
        let root = workspace("unknown-fields");
        fs::write(root.join("text.txt"), "one\n").expect("fixture");
        let read = prepare(&root, "read", json!({"path":"text.txt", "extra":true}));
        assert!(read.error.is_some());
        let patch = prepare(
            &root,
            "patch",
            json!({"path":"text.txt", "edits":[{"expected":"one", "replacement":"two", "extra":true}]}),
        );
        assert!(patch.error.is_some());
        let legacy = prepare(
            &root,
            "patch",
            json!({"path":"text.txt", "expected":"one", "replacement":"two"}),
        );
        let batch = prepare(
            &root,
            "patch",
            json!({"path":"text.txt", "edits":[{"expected":"one", "replacement":"two"}]}),
        );
        assert!(legacy.error.is_none(), "{:?}", legacy.error);
        assert_eq!(legacy.canonical_fingerprint, batch.canonical_fingerprint);
        let mcp = prepare(&root, "mcp", json!({"server":"fixture", "extra":true}));
        assert!(
            mcp.error.is_none(),
            "MCP payload was filtered: {:?}",
            mcp.error
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn context_cursor_timeout_and_code_intel_use_effective_identity() {
        let root = workspace("effective-values");
        fs::write(root.join("text.txt"), "needle\ncontext\n").expect("fixture");

        let capped = prepare(
            &root,
            "search",
            json!({"path":"text.txt", "query":"needle", "context_lines":10}),
        );
        let explicit_cap = prepare(
            &root,
            "search",
            json!({"path":"text.txt", "query":"needle", "context_lines":3}),
        );
        assert!(capped.error.is_none(), "{:?}", capped.error);
        assert_eq!(
            capped.canonical_fingerprint,
            explicit_cap.canonical_fingerprint
        );
        assert!(capped
            .admission_notes
            .iter()
            .any(|note| note.contains("context_lines") && note.contains("10")));
        let negative = prepare(
            &root,
            "search",
            json!({"path":"text.txt", "query":"needle", "context_lines":-1}),
        );
        assert!(negative.error.is_some());

        let blank_cursor = prepare(&root, "list", json!({"path":".", "cursor":"  "}));
        let no_cursor = prepare(&root, "list", json!({"path":"."}));
        assert_eq!(
            blank_cursor.canonical_fingerprint,
            no_cursor.canonical_fingerprint
        );
        let trimmed_cursor = prepare(
            &root,
            "search",
            json!({"path":"text.txt", "query":"needle", "cursor":"  token  "}),
        );
        let canonical_cursor = prepare(
            &root,
            "search",
            json!({"path":"text.txt", "query":"needle", "cursor":"token"}),
        );
        assert_eq!(
            trimmed_cursor.canonical_fingerprint,
            canonical_cursor.canonical_fingerprint
        );

        for timeout in [json!(0), json!(120_001)] {
            let rejected = prepare(
                &root,
                "shell",
                json!({"command":"echo", "timeout_ms":timeout}),
            );
            assert!(
                rejected.error.is_some(),
                "accepted invalid timeout: {timeout}"
            );
        }

        fs::write(root.join("lib.rs"), "fn target() {}\n").expect("code fixture");
        let max_zero = prepare(
            &root,
            "code_intel",
            json!({"action":"symbol", "path":"lib.rs", "max_results":0}),
        );
        let max_one = prepare(
            &root,
            "code_intel",
            json!({"action":"symbol", "path":"lib.rs", "max_results":1}),
        );
        assert!(max_zero.error.is_none(), "{:?}", max_zero.error);
        assert_eq!(
            max_zero.canonical_fingerprint,
            max_one.canonical_fingerprint
        );
        let max_default = prepare(
            &root,
            "code_intel",
            json!({"action":"symbol", "path":"lib.rs"}),
        );
        let max_twenty = prepare(
            &root,
            "code_intel",
            json!({"action":"symbol", "path":"lib.rs", "max_results":20}),
        );
        assert_eq!(
            max_default.canonical_fingerprint,
            max_twenty.canonical_fingerprint
        );
        let symbol_query = prepare(
            &root,
            "code_intel",
            json!({"action":"symbol", "query":"target"}),
        );
        let symbol_query_null_path = prepare(
            &root,
            "code_intel",
            json!({"action":"symbol", "path":null, "query":"target"}),
        );
        assert_eq!(
            symbol_query.canonical_fingerprint,
            symbol_query_null_path.canonical_fingerprint
        );
        let status = prepare(&root, "code_intel", json!({"action":"status"}));
        let status_with_ignored = prepare(
            &root,
            "code_intel",
            json!({"action":"status", "max_results":20, "include_info":false, "query":"ignored"}),
        );
        assert_eq!(
            status.canonical_fingerprint,
            status_with_ignored.canonical_fingerprint
        );
        let diagnostics = prepare(
            &root,
            "code_intel",
            json!({"action":"diagnostics", "path":"lib.rs", "offset":0, "revision":null}),
        );
        let diagnostics_canonical = prepare(
            &root,
            "code_intel",
            json!({"action":"diagnostics", "path":"lib.rs"}),
        );
        assert_eq!(
            diagnostics.canonical_fingerprint,
            diagnostics_canonical.canonical_fingerprint
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shell_admission_guides_common_model_slips() {
        let root = workspace("shell-admission");

        let payload = prepare(
            &root,
            "shell",
            json!({"command":"{\"args\":[\"-NoProfile\",\"-Command\",\"echo hi\"],\"timeout_ms\":15000}"}),
        );
        assert_eq!(
            payload.error.as_deref(),
            Some(
                "shell command must be a command line, not a JSON tool payload; pass the command text in `command`"
            )
        );

        let program_line = prepare(
            &root,
            "shell",
            json!({"command":"python check.py", "args":[]}),
        );
        assert!(program_line.error.is_none(), "{:?}", program_line.error);
        assert!(program_line
            .admission_notes
            .iter()
            .any(|note| note.contains("executable name alone")));
        let program_path = prepare(
            &root,
            "shell",
            json!({"command":"C:\\Program Files\\tool\\run.exe", "args":["-v"]}),
        );
        assert!(
            program_path.admission_notes.is_empty(),
            "{:?}",
            program_path.admission_notes
        );

        let bash = prepare(&root, "shell", json!({"command":"ls -la | head -5"}));
        assert!(bash.error.is_none(), "{:?}", bash.error);
        assert!(bash
            .admission_notes
            .iter()
            .any(|note| note.contains("bash syntax")));
        let powershell = prepare(
            &root,
            "shell",
            json!({"command":"Get-ChildItem | Select-Object -First 5"}),
        );
        assert!(powershell.error.is_none());
        assert!(powershell.admission_notes.is_empty());
        let script_block = prepare(&root, "shell", json!({"command":"{ echo hi }"}));
        assert!(script_block.error.is_none(), "{:?}", script_block.error);
        let bash_literal = prepare(&root, "shell", json!({"command":"Write-Output 'head -5'"}));
        assert!(bash_literal.error.is_none(), "{:?}", bash_literal.error);
        assert!(
            bash_literal
                .admission_notes
                .iter()
                .any(|note| note.contains("possible") && note.contains("compatibility/quoting")),
            "{:?}",
            bash_literal.admission_notes
        );

        let inline_eval = prepare(
            &root,
            "shell",
            json!({"command":"python -c \"import sys\nprint(sys.argv)\""}),
        );
        assert!(inline_eval.error.is_none(), "{:?}", inline_eval.error);
        assert!(
            inline_eval
                .admission_notes
                .iter()
                .any(|note| note.contains("possible quoting risk")
                    && note.contains("if parsing fails")),
            "{:?}",
            inline_eval.admission_notes
        );
        assert!(
            inline_eval
                .admission_notes
                .iter()
                .all(|note| !note.contains("breaks quoting")),
            "{:?}",
            inline_eval.admission_notes
        );
        let single_line_eval = prepare(&root, "shell", json!({"command":"python -c \"print(1)\""}));
        assert!(single_line_eval.admission_notes.is_empty());
        let multiline_script =
            prepare(&root, "shell", json!({"command":"$x = 1\nWrite-Output $x"}));
        assert!(multiline_script.admission_notes.is_empty());
        let multiline_non_eval = prepare(
            &root,
            "shell",
            json!({"command":"Write-Output 'head -5'\nWrite-Output done"}),
        );
        assert!(
            multiline_non_eval.error.is_none(),
            "{:?}",
            multiline_non_eval.error
        );
        assert!(
            multiline_non_eval
                .admission_notes
                .iter()
                .all(|note| !note.contains("quoting risk")),
            "{:?}",
            multiline_non_eval.admission_notes
        );
        let eval_program_form = prepare(
            &root,
            "shell",
            json!({"command":"python", "args":["-c", "import sys\nprint(sys.argv)"]}),
        );
        assert!(eval_program_form.admission_notes.is_empty());

        let _ = fs::remove_dir_all(root);
    }
}
