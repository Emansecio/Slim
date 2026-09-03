use super::{
    code_intel::parse_prepared_code_intel_request, code_intel::CodeIntelRequest,
    resolve_workspace_path_from_root, ToolCacheability, ToolDependencyScope, ToolEffectClass,
    ToolError, ToolOperationalSpec, ToolReplayPolicy, ToolResult, ToolVolatility,
    DEFAULT_MAX_ENTRIES, DEFAULT_MAX_HITS, DEFAULT_MAX_READ_LINES, MAX_ENTRIES_CAP, MAX_HITS_CAP,
    MAX_READ_LINES_CAP, MAX_SEARCH_PATTERNS,
};
use crate::codeintel::DEFAULT_CODE_INTEL_LIMIT;
use crate::OperatingMode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::{File, Metadata};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

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
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return false;
        };
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos());
        match self.stamp.kind {
            DependencyKind::File => {
                metadata.len() == self.stamp.len && modified_nanos == self.stamp.modified_nanos
            }
            DependencyKind::Directory => {
                modified_nanos == self.stamp.modified_nanos && self.stamp.modified_nanos.is_some()
            }
        }
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
        max_lines: usize,
    },
    List {
        offset: usize,
        max_entries: usize,
        cursor: Option<String>,
    },
    Search {
        patterns: Vec<String>,
        offset: usize,
        max_hits: usize,
        cursor: Option<String>,
    },
    Write {
        content: String,
        expected: Option<String>,
    },
    Patch {
        expected: String,
        replacement: String,
    },
    Shell {
        command: String,
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
    pub(crate) canonical_workspace: PathBuf,
    pub(crate) target_paths: Vec<PathBuf>,
    pub(crate) spec: Option<ToolOperationalSpec>,
    pub(crate) canonical_fingerprint: String,
    pub(crate) preparation_us: u64,
    pub(crate) error: Option<String>,
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
        if error.is_none() {
            error = workspace_error;
        }
        let mut target_paths = Vec::new();
        if error.is_none() {
            if let Err(message) = materialize_defaults_and_paths(
                name,
                &canonical_workspace,
                &mut arguments,
                &mut target_paths,
            ) {
                error = Some(message);
            }
        }
        let spec = effective_spec(native_spec, name, &arguments);
        let canonical_arguments = canonical_json(&arguments);
        let typed_arguments = if error.is_none() {
            match typed_arguments(name, &arguments, &canonical_workspace, &target_paths) {
                Ok(arguments) => arguments,
                Err(message) => {
                    error = Some(message);
                    PreparedToolArguments::External
                }
            }
        } else {
            PreparedToolArguments::External
        };
        let canonical_fingerprint = spec.map_or_else(
            || {
                hash_fields(&[
                    b"slim-prepared-call-v1",
                    name.as_bytes(),
                    canonical_arguments.as_bytes(),
                ])
            },
            |spec| {
                hash_fields(&[
                    b"slim-prepared-call-v1",
                    name.as_bytes(),
                    canonical_arguments.as_bytes(),
                    format!("{:?}", spec.effect_class).as_bytes(),
                    format!("{:?}", spec.cacheability).as_bytes(),
                    format!("{:?}", spec.volatility).as_bytes(),
                    format!("{:?}", spec.dependency_scope).as_bytes(),
                    format!("{:?}", spec.replay_policy).as_bytes(),
                ])
            },
        );
        Self {
            mode,
            name: name.to_owned(),
            arguments: typed_arguments,
            canonical_workspace,
            target_paths,
            spec,
            canonical_fingerprint,
            preparation_us: workspace_preparation_us.saturating_add(elapsed_us(started)),
            error,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolExecutionReceipt {
    pub(crate) dependencies: Vec<DependencyObservation>,
    pub(crate) mutations: Vec<MutationObservation>,
    pub(crate) modified_paths: Vec<PathBuf>,
    pub(crate) revision_before: u64,
    pub(crate) revision_after: u64,
    pub(crate) bytes_read: u64,
    pub(crate) preparation_us: u64,
    pub(crate) execution_us: u64,
    pub(crate) finalization_us: u64,
    pub(crate) synced_text: Option<String>,
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
            revision_before,
            revision_after,
            bytes_read: 0,
            preparation_us: prepared.preparation_us,
            execution_us,
            finalization_us: 0,
            synced_text: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ToolExecutionError {
    pub(crate) error: ToolError,
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

#[derive(Clone, Debug, Eq, PartialEq)]
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

fn materialize_defaults_and_paths(
    tool_name: &str,
    cwd: &Path,
    arguments: &mut Value,
    target_paths: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let object = arguments
        .as_object_mut()
        .ok_or_else(|| "tool arguments must be a JSON object".to_owned())?;
    match tool_name {
        "read" => {
            insert_default(object, "offset", Value::from(1));
            insert_default(
                object,
                "max_lines",
                Value::from(u64::try_from(DEFAULT_MAX_READ_LINES).unwrap_or(u64::MAX)),
            );
        }
        "list" => {
            insert_default_path(object, ".");
            insert_default(object, "offset", Value::from(1));
            insert_default(
                object,
                "max_entries",
                Value::from(u64::try_from(DEFAULT_MAX_ENTRIES).unwrap_or(u64::MAX)),
            );
        }
        "search" => {
            insert_default_path(object, ".");
            insert_default(object, "offset", Value::from(1));
            insert_default(
                object,
                "max_hits",
                Value::from(u64::try_from(DEFAULT_MAX_HITS).unwrap_or(u64::MAX)),
            );
            match (has_search_query(object), has_search_patterns(object)) {
                (false, false) => {
                    return Err("search requires exactly one of query or patterns".into());
                }
                (true, true) | (true, false) => {
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

fn typed_arguments(
    tool_name: &str,
    arguments: &Value,
    canonical_workspace: &Path,
    target_paths: &[PathBuf],
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
            let max_lines = integer_argument(arguments, "max_lines")?;
            let offset = integer_argument(arguments, "offset")?;
            if max_lines == 0 || max_lines > MAX_READ_LINES_CAP {
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
            Ok(PreparedToolArguments::Search {
                patterns,
                offset,
                max_hits,
                cursor: optional_nonempty_string_argument(arguments, "cursor")?,
            })
        }
        "write" => Ok(PreparedToolArguments::Write {
            content: string_argument("content")?,
            expected: optional_string_argument(arguments, "expected")?,
        }),
        "patch" => Ok(PreparedToolArguments::Patch {
            expected: nonempty_string_argument("expected")?,
            replacement: string_argument("replacement")?,
        }),
        "shell" => Ok(PreparedToolArguments::Shell {
            command: nonempty_string_argument("command")?,
            timeout_ms: u64_argument(arguments, "timeout_ms")?.clamp(1, 120_000),
        }),
        "code_intel" => Ok(PreparedToolArguments::CodeIntel(
            parse_prepared_code_intel_request(
                canonical_workspace,
                arguments,
                target_paths.first().map(PathBuf::as_path),
            )?,
        )),
        _ => Ok(PreparedToolArguments::External),
    }
}

fn integer_argument(arguments: &Value, name: &str) -> Result<usize, String> {
    let value = u64_argument(arguments, name)?;
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

fn insert_default_path(object: &mut serde_json::Map<String, Value>, default: &str) {
    let blank = object
        .get("path")
        .and_then(Value::as_str)
        .is_none_or(|path| path.trim().is_empty());
    if blank {
        object.insert("path".into(), Value::from(default));
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
    if command.chars().any(|character| {
        matches!(
            character,
            ';' | '|' | '&' | '>' | '<' | '`' | '$' | '\n' | '\r'
        )
    }) {
        return false;
    }
    let tokens = command.split_whitespace().collect::<Vec<_>>();
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
