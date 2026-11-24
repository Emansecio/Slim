//! Workspace file discovery and confined `@mention` loading for the TUI.
//!
//! Both entry points stay inside the canonical workspace root: listing never
//! follows links, and loading resolves through the same confinement helper the
//! native tools use, so `..`, absolute paths and links leaving the workspace
//! are rejected.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;

use crate::runtime::CancellationToken;
use crate::tools::{is_macos_metadata_file, resolve_workspace_path_from_root, SKIP_DIR_NAMES};

/// Listing is a completion aid; it must never block the caller for long.
const LIST_TIME_BUDGET: Duration = Duration::from_millis(1500);
/// A NUL byte in this prefix marks the file as binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Candidate cap per prompt so a pasted wall of `@` tokens cannot stat-storm.
const MAX_MENTION_CANDIDATES: usize = 64;
const MAX_MENTION_TOKEN_BYTES: usize = 1024;
const MAX_MENTION_SOURCE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MentionSelector {
    LineRange { start: usize, end: usize },
    Symbol(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MentionSpec {
    pub raw: String,
    pub path: String,
    pub selector: Option<MentionSelector>,
}

/// Files found by [`list_workspace_files`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceListing {
    /// Workspace-relative, forward-slash paths, shallower-first then lexicographic.
    pub files: Vec<String>,
    /// The time budget ran out before the walk finished, so `files` is a
    /// partial listing even when it is shorter than the requested limit.
    pub timed_out: bool,
}

/// Files found by [`glob_workspace_files`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct GlobListing {
    pub files: Vec<String>,
    pub truncated: bool,
    pub timed_out: bool,
}

const GLOB_MAX_ENTRIES: usize = 1000;
const GLOB_SCAN_LIMIT: usize = 20_000;
const GLOB_TIME_BUDGET: Duration = Duration::from_secs(5);

pub(crate) fn glob_workspace_files(
    canonical_workspace: &Path,
    sub_path: Option<&str>,
    pattern: Option<&str>,
    max_entries: usize,
    cancel: Option<&CancellationToken>,
) -> Result<GlobListing, String> {
    if !(1..=GLOB_MAX_ENTRIES).contains(&max_entries) {
        return Err(format!("glob max_entries must be 1-{GLOB_MAX_ENTRIES}"));
    }
    let search_root = match sub_path {
        None | Some(".") => canonical_workspace.to_path_buf(),
        Some("") => return Err("path must not be empty".into()),
        Some(sub_path) => {
            let resolved = resolve_workspace_path_from_root(canonical_workspace, sub_path)?;
            let metadata = fs::metadata(&resolved).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    "glob path does not exist".to_owned()
                } else {
                    format!("glob path cannot be read: {error}")
                }
            })?;
            if !metadata.is_dir() {
                return Err("glob path is not a directory".into());
            }
            resolved
        }
    };
    let pattern = pattern.unwrap_or("*");
    if pattern.trim().is_empty() || pattern.len() > 512 {
        return Err("glob pattern must contain 1-512 bytes".into());
    }
    if pattern.starts_with('!') {
        return Err("glob pattern must be an include pattern, not negated with `!`".into());
    }
    if pattern.starts_with('/') || pattern.split(['/', '\\']).any(|segment| segment == "..") {
        return Err("glob pattern must stay relative inside the workspace".into());
    }
    let mut override_builder = ignore::overrides::OverrideBuilder::new(&search_root);
    override_builder
        .add(pattern)
        .map_err(|error| format!("invalid glob pattern: {error}"))?;
    let overrides = override_builder
        .build()
        .map_err(|error| format!("invalid glob pattern: {error}"))?;
    if overrides.num_whitelists() == 0 {
        return Err("glob pattern must be a positive include pattern".into());
    }

    let started = Instant::now();
    let cancelled = || cancel.is_some_and(CancellationToken::is_cancelled);

    // Fast path: match the glob against the ProjectIndexDaemon's in-memory file
    // list (already sorted by depth then path) with early exit at `max_entries`.
    if let Ok(daemon) = crate::project_index::for_workspace(canonical_workspace) {
        let (tree, _) = daemon.all_files_arc();
        if !tree.truncated {
            let sub_prefix = search_root
                .strip_prefix(canonical_workspace)
                .ok()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| {
                    let s = p.to_string_lossy().replace('\\', "/");
                    if s.ends_with('/') {
                        s
                    } else {
                        format!("{s}/")
                    }
                });
            let mut files: Vec<String> = Vec::with_capacity(max_entries.min(64));
            let mut seen_files = 0usize;
            let mut truncated = false;
            for (idx, rel) in tree.files.iter().enumerate() {
                if idx & 255 == 0 && cancelled() {
                    return Err("glob cancelled".into());
                }
                let rel_in_scope = if let Some(prefix) = &sub_prefix {
                    let Some(rest) = rel.strip_prefix(prefix.as_str()) else {
                        continue;
                    };
                    rest
                } else {
                    rel.as_str()
                };
                seen_files += 1;
                if seen_files > GLOB_SCAN_LIMIT {
                    truncated = true;
                    break;
                }
                if overrides.matched(rel_in_scope, false).is_whitelist() {
                    if files.len() >= max_entries {
                        truncated = true;
                        break;
                    }
                    files.push(rel.clone());
                }
            }
            return Ok(GlobListing {
                files,
                truncated,
                timed_out: false,
            });
        }
    }

    let mut builder = WalkBuilder::new(&search_root);
    builder
        .require_git(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .hidden(false)
        .follow_links(false)
        .sort_by_file_name(|left, right| left.cmp(right));
    builder.filter_entry(|entry| {
        if entry.depth() == 0 {
            return true;
        }
        let Some(kind) = entry.file_type() else {
            return false;
        };
        if kind.is_symlink() {
            return false;
        }
        if kind.is_dir() {
            return !entry
                .file_name()
                .to_str()
                .is_some_and(|name| SKIP_DIR_NAMES.contains(&name))
                && !is_reparse_point(entry.path());
        }
        !entry
            .file_name()
            .to_str()
            .is_some_and(is_macos_metadata_file)
    });

    let mut files: Vec<(usize, String)> = Vec::new();
    let mut timed_out = false;
    let mut seen_files = 0usize;
    let mut visited = 0usize;
    let mut hit_scan_cap = false;
    for entry in builder.build() {
        if cancelled() {
            return Err("glob cancelled".into());
        }
        visited += 1;
        if visited & 63 == 0 && started.elapsed() >= GLOB_TIME_BUDGET {
            timed_out = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        seen_files += 1;
        if seen_files > GLOB_SCAN_LIMIT {
            hit_scan_cap = true;
            break;
        }
        if overrides.matched(entry.path(), false).is_whitelist() {
            if let Some(relative) = relative_slash_path(canonical_workspace, entry.path()) {
                let depth = relative.bytes().filter(|byte| *byte == b'/').count();
                files.push((depth, relative));
            }
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let truncated = files.len() > max_entries || hit_scan_cap;
    files.truncate(max_entries);
    let files = files.into_iter().map(|(_, path)| path).collect();
    Ok(GlobListing {
        files,
        truncated,
        timed_out,
    })
}

const WORKSPACE_LISTING_CACHE_CAP: usize = 8;
const WORKSPACE_LISTING_CACHE_TTL: Duration = Duration::from_secs(2);

struct CachedWorkspaceListing {
    canonical_workspace: PathBuf,
    max_entries: usize,
    stored_at: Instant,
    listing: WorkspaceListing,
}

static WORKSPACE_LISTING_CACHE: OnceLock<Mutex<VecDeque<CachedWorkspaceListing>>> = OnceLock::new();

fn listing_cache() -> &'static Mutex<VecDeque<CachedWorkspaceListing>> {
    WORKSPACE_LISTING_CACHE.get_or_init(|| Mutex::new(VecDeque::with_capacity(WORKSPACE_LISTING_CACHE_CAP)))
}

/// Drops any cached [`list_workspace_files`] results for `canonical_workspace`.
pub fn invalidate_workspace_index(canonical_workspace: &Path) {
    if let Some(daemon) = crate::project_index::existing_for_workspace(canonical_workspace) {
        daemon.invalidate_workspace();
    }
    let mut guard = match listing_cache().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.retain(|entry| entry.canonical_workspace != canonical_workspace);
}

/// Workspace-relative, forward-slash paths of regular files honoring
/// `.gitignore`, skipping links, Windows junctions, the directories in
/// `SKIP_DIR_NAMES` and macOS litter (`.DS_Store`, `._*`). Dotfiles are
/// included. The result is bounded by `limit` and a time budget (partial
/// output is returned when either is hit; only the budget sets
/// [`WorkspaceListing::timed_out`]), sorted shallower-first then
/// lexicographically. Cancellation yields `ErrorKind::Interrupted`.
pub fn list_workspace_files(
    root: &Path,
    limit: usize,
    cancel: Option<&CancellationToken>,
) -> io::Result<WorkspaceListing> {
    if limit == 0 {
        return Ok(WorkspaceListing::default());
    }
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    let canonical = fs::canonicalize(root)?;
    if let Ok(daemon) = crate::project_index::for_workspace(&canonical) {
        let (files, _) = daemon.snapshot_files(limit);
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        return Ok(WorkspaceListing {
            files,
            timed_out: false,
        });
    }
    {
        let mut guard = match listing_cache().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.retain(|entry| entry.stored_at.elapsed() < WORKSPACE_LISTING_CACHE_TTL);
        if let Some(hit) = guard
            .iter()
            .find(|entry| entry.canonical_workspace == canonical && entry.max_entries == limit)
        {
            return Ok(hit.listing.clone());
        }
    }
    let listing = list_with_budget(&canonical, limit, cancel, LIST_TIME_BUDGET)?;
    if !listing.timed_out && !cancel.is_some_and(CancellationToken::is_cancelled) {
        let mut guard = match listing_cache().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.retain(|entry| {
            entry.stored_at.elapsed() < WORKSPACE_LISTING_CACHE_TTL
                && !(entry.canonical_workspace == canonical && entry.max_entries == limit)
        });
        if guard.len() >= WORKSPACE_LISTING_CACHE_CAP {
            guard.pop_front();
        }
        guard.push_back(CachedWorkspaceListing {
            canonical_workspace: canonical,
            max_entries: limit,
            stored_at: Instant::now(),
            listing: listing.clone(),
        });
    }
    Ok(listing)
}

fn list_with_budget(
    root: &Path,
    limit: usize,
    cancel: Option<&CancellationToken>,
    budget: Duration,
) -> io::Result<WorkspaceListing> {
    let started = Instant::now();
    let cancelled = || cancel.is_some_and(CancellationToken::is_cancelled);
    let root = fs::canonicalize(root)?;
    if limit == 0 {
        return Ok(WorkspaceListing::default());
    }
    let mut builder = WalkBuilder::new(&root);
    builder
        .require_git(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .hidden(false)
        .follow_links(false)
        .sort_by_file_name(|left, right| left.cmp(right));
    builder.filter_entry(|entry| {
        if entry.depth() == 0 {
            return true;
        }
        let Some(kind) = entry.file_type() else {
            return false;
        };
        if kind.is_symlink() {
            return false;
        }
        if kind.is_dir() {
            return !entry
                .file_name()
                .to_str()
                .is_some_and(|name| SKIP_DIR_NAMES.contains(&name))
                && !is_reparse_point(entry.path());
        }
        !entry
            .file_name()
            .to_str()
            .is_some_and(is_macos_metadata_file)
    });

    let mut files = Vec::new();
    let mut timed_out = false;
    for entry in builder.build() {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace listing cancelled",
            ));
        }
        if files.len() >= limit {
            break;
        }
        if started.elapsed() >= budget {
            timed_out = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Some(relative) = relative_slash_path(&root, entry.path()) {
            files.push(relative);
        }
    }
    files.sort_by(|left, right| {
        let depth = |path: &str| path.bytes().filter(|byte| *byte == b'/').count();
        (depth(left), left).cmp(&(depth(right), right))
    });
    Ok(WorkspaceListing { files, timed_out })
}

/// Files enumerated when looking for a missing path's name elsewhere.
const SAME_NAME_SCAN_FILES: usize = 20_000;
const MAX_SAME_NAME_FILES: usize = 3;

/// A note naming workspace files that share the file name of `missing`, for a
/// tool call that got the directory wrong. `None` when there is none or the
/// listing fails: this is a hint, never the reason a call fails. Files with a
/// sensitive name (see [`is_sensitive_file_name`]) are never suggested.
pub(crate) fn same_name_files_note(
    root: &Path,
    missing: &Path,
    cancel: Option<&CancellationToken>,
) -> Option<String> {
    let name = missing.file_name()?.to_str()?;
    let found = list_workspace_files(root, SAME_NAME_SCAN_FILES, cancel)
        .ok()?
        .files
        .into_iter()
        .filter(|path| {
            path.rsplit('/').next().is_some_and(|file| {
                file.eq_ignore_ascii_case(name) && !is_sensitive_file_name(file)
            })
        })
        .take(MAX_SAME_NAME_FILES)
        .collect::<Vec<_>>();
    (!found.is_empty()).then(|| format!("Same file name elsewhere: {}", found.join(", ")))
}

/// Text of a file mentioned with `@path`, capped at the caller's byte budget.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MentionFile {
    /// Workspace-relative, forward-slash path of the resolved file.
    pub path: String,
    /// UTF-8 text without a leading BOM, cut at a char boundary when truncated.
    pub text: String,
    /// Length of `text` in bytes.
    pub bytes: usize,
    /// True when the file holds more data than `max_bytes` allowed.
    pub truncated: bool,
}

#[derive(Debug)]
pub enum MentionError {
    Empty,
    NotFound,
    OutsideWorkspace,
    NotRegularFile,
    Binary,
    Sensitive,
    Io(String),
}

impl fmt::Display for MentionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("mention path is empty"),
            Self::NotFound => formatter.write_str("file not found"),
            Self::OutsideWorkspace => formatter.write_str("path is outside the workspace"),
            Self::NotRegularFile => formatter.write_str("not a regular file"),
            Self::Binary => formatter.write_str("file is binary or not valid UTF-8"),
            Self::Sensitive => formatter.write_str("file name looks like a secret"),
            Self::Io(message) => write!(formatter, "io error: {message}"),
        }
    }
}

impl std::error::Error for MentionError {}

/// Load one workspace file for a mention, reading at most `max_bytes` bytes.
/// Non-UTF-8 content, NUL bytes in the first 8 KiB and secret-looking file
/// names (see [`is_sensitive_file_name`]) are refused.
pub fn load_mention_file(
    root: &Path,
    relative: &str,
    max_bytes: usize,
) -> Result<MentionFile, MentionError> {
    let root = fs::canonicalize(root).map_err(|error| MentionError::Io(error.to_string()))?;
    let (resolved, display) = resolve_regular_file(&root, relative)?;
    let requested_name = Path::new(relative)
        .file_name()
        .and_then(|name| name.to_str());
    let resolved_name = resolved.file_name().and_then(|name| name.to_str());
    if requested_name
        .into_iter()
        .chain(resolved_name)
        .any(is_sensitive_file_name)
    {
        return Err(MentionError::Sensitive);
    }

    let file = File::open(&resolved).map_err(map_io_error)?;
    if !file.metadata().map_err(map_io_error)?.is_file() {
        return Err(MentionError::NotRegularFile);
    }
    let mut buffer = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut buffer)
        .map_err(map_io_error)?;
    let truncated = buffer.len() > max_bytes;
    buffer.truncate(max_bytes);
    if buffer
        .iter()
        .take(BINARY_SNIFF_BYTES)
        .any(|byte| *byte == 0)
    {
        return Err(MentionError::Binary);
    }
    let valid_len = match std::str::from_utf8(&buffer) {
        Ok(_) => buffer.len(),
        // Only a sequence cut by the byte cap may be dropped; any other
        // invalid byte means the file is not text.
        Err(error) if truncated && error.error_len().is_none() => error.valid_up_to(),
        Err(_) => return Err(MentionError::Binary),
    };
    buffer.truncate(valid_len);
    let text = String::from_utf8(buffer).map_err(|_| MentionError::Binary)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
    Ok(MentionFile {
        path: display,
        bytes: text.len(),
        text,
        truncated,
    })
}

/// Case-insensitive file-name check for obvious secrets: `.env` and
/// `.env.*` (except `.example`/`.sample`/`.template`), `*.pem`, `*.key`,
/// `*.p12`, `*.pfx`, `*.kdbx`, `id_rsa*`, `id_ed25519*`, `credentials.json`,
/// `.npmrc`, `.pypirc` and `auth.json`.
pub fn is_sensitive_file_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name == ".env" {
        return true;
    }
    if let Some(suffix) = name.strip_prefix(".env.") {
        return !matches!(suffix, "example" | "sample" | "template");
    }
    [".pem", ".key", ".p12", ".pfx", ".kdbx"]
        .iter()
        .any(|extension| name.ends_with(extension))
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || matches!(
            name.as_str(),
            "credentials.json" | ".npmrc" | ".pypirc" | "auth.json"
        )
}

/// Workspace-relative paths of the existing regular files named by `@token`s
/// in `prompt`. A mention starts at the beginning of the text or after
/// whitespace or an opening bracket/quote; trailing punctuation is stripped
/// unless the file with that exact name exists. Unique, in prompt order;
/// tokens that do not resolve to a file (`@override`, `a@b.com`) are ignored.
pub fn mention_paths_in_prompt(root: &Path, prompt: &str) -> Vec<String> {
    let Ok(root) = fs::canonicalize(root) else {
        return Vec::new();
    };
    let mut found = Vec::<String>::new();
    let mut candidates = 0;
    let mut previous: Option<char> = None;
    for (index, character) in prompt.char_indices() {
        let starts_mention = character == '@'
            && previous.is_none_or(|before| {
                before.is_whitespace() || matches!(before, '(' | '[' | '{' | '<' | '"' | '\'' | '`')
            });
        previous = Some(character);
        if !starts_mention {
            continue;
        }
        let rest = &prompt[index + 1..];
        let token = &rest[..rest.find(char::is_whitespace).unwrap_or(rest.len())];
        if token.is_empty() || token.len() > MAX_MENTION_TOKEN_BYTES {
            continue;
        }
        let mut candidate = token;
        loop {
            if candidates == MAX_MENTION_CANDIDATES {
                return found;
            }
            candidates += 1;
            if let Ok((_, display)) = resolve_regular_file(&root, candidate) {
                if !found.contains(&display) {
                    found.push(display);
                }
                break;
            }
            match candidate.strip_suffix(is_trailing_punctuation) {
                Some(shorter) if !shorter.is_empty() => candidate = shorter,
                _ => break,
            }
        }
    }
    found
}

fn parse_mention_selector(candidate: &str) -> (&str, Option<MentionSelector>) {
    if let Some((path, frag)) = candidate.rsplit_once('#') {
        if let Some(rest) = frag.strip_prefix('L').or_else(|| frag.strip_prefix('l')) {
            if let Some((a, b)) = rest.split_once('-') {
                let b_num = b.strip_prefix('L').or_else(|| b.strip_prefix('l')).unwrap_or(b);
                if let (Ok(start), Ok(end)) = (a.parse::<usize>(), b_num.parse::<usize>()) {
                    if start >= 1 && end >= start {
                        return (path, Some(MentionSelector::LineRange { start, end }));
                    }
                }
            } else if let Ok(line) = rest.parse::<usize>() {
                if line >= 1 {
                    return (
                        path,
                        Some(MentionSelector::LineRange {
                            start: line,
                            end: line,
                        }),
                    );
                }
            }
        }
    }
    if let Some((path, sym)) = candidate.rsplit_once("::") {
        let sym = sym.trim().trim_end_matches(is_trailing_punctuation);
        if !path.is_empty() && !sym.is_empty() {
            return (path, Some(MentionSelector::Symbol(sym.to_owned())));
        }
    }
    (candidate, None)
}

/// Like [`mention_paths_in_prompt`], but also recognizes `#L<start>-L<end>`,
/// `#L<line>`, and `::<symbol>` selectors on workspace files.
pub fn mention_specs_in_prompt(root: &Path, prompt: &str) -> Vec<MentionSpec> {
    let Ok(root) = fs::canonicalize(root) else {
        return Vec::new();
    };
    let mut found = Vec::<MentionSpec>::new();
    let mut candidates = 0;
    let mut previous: Option<char> = None;
    for (index, character) in prompt.char_indices() {
        let starts_mention = character == '@'
            && previous.is_none_or(|before| {
                before.is_whitespace() || matches!(before, '(' | '[' | '{' | '<' | '"' | '\'' | '`')
            });
        previous = Some(character);
        if !starts_mention {
            continue;
        }
        let rest = &prompt[index + 1..];
        let token = &rest[..rest.find(char::is_whitespace).unwrap_or(rest.len())];
        if token.is_empty() || token.len() > MAX_MENTION_TOKEN_BYTES {
            continue;
        }
        let mut candidate = token;
        loop {
            if candidates == MAX_MENTION_CANDIDATES {
                return found;
            }
            candidates += 1;
            if let Ok((_, display)) = resolve_regular_file(&root, candidate) {
                let spec = MentionSpec {
                    raw: candidate.to_owned(),
                    path: display,
                    selector: None,
                };
                if !found
                    .iter()
                    .any(|s| s.path == spec.path && s.selector == spec.selector)
                {
                    found.push(spec);
                }
                break;
            }
            let (base, selector) = parse_mention_selector(candidate);
            if selector.is_some() {
                if let Ok((_, display)) = resolve_regular_file(&root, base) {
                    let spec = MentionSpec {
                        raw: candidate.to_owned(),
                        path: display,
                        selector,
                    };
                    if !found
                        .iter()
                        .any(|s| s.path == spec.path && s.selector == spec.selector)
                    {
                        found.push(spec);
                    }
                    break;
                }
            }
            match candidate.strip_suffix(is_trailing_punctuation) {
                Some(shorter) if !shorter.is_empty() => candidate = shorter,
                _ => break,
            }
        }
    }
    found
}

const MENTION_DISTILL_LINES: usize = 300;
const MENTION_DISTILL_BYTES: usize = 16 * 1024;
const MENTION_PREVIEW_HEAD_LINES: usize = 60;
const MENTION_PREVIEW_TAIL_LINES: usize = 40;

/// Loads a resolved [`MentionSpec`], slicing only the requested line range or
/// extracted symbol when a selector is present. Large whole-file mentions
/// (`> 300` lines or `> 16 KiB` without a selector) are deterministically
/// distilled into structural outlines (or JSON tables / head+tail) to avoid
/// flooding the prompt context.
pub fn load_mention_spec(
    root: &Path,
    spec: &MentionSpec,
    max_bytes: usize,
) -> Result<MentionFile, MentionError> {
    let Some(selector) = &spec.selector else {
        let full = load_mention_file(root, &spec.path, max_bytes)?;
        let line_count = full.text.lines().count();
        if line_count <= MENTION_DISTILL_LINES && full.text.len() <= MENTION_DISTILL_BYTES {
            return Ok(full);
        }
        // Try structural outline (fast path via ProjectIndexDaemon slab when available).
        let syms = crate::project_index::for_workspace(root)
            .ok()
            .and_then(|d| {
                let abs = d.canonical_root().join(&full.path);
                d.read_file_cached(&abs).ok().flatten().map(|f| f.outline())
            })
            .unwrap_or_else(|| {
                std::sync::Arc::from(crate::tools::outline::extract_outline(
                    Path::new(&full.path),
                    &full.text,
                ))
            });
        if !syms.is_empty() {
            let head: String = full
                .text
                .lines()
                .take(MENTION_PREVIEW_HEAD_LINES)
                .enumerate()
                .map(|(i, l)| format!("{}: {l}\n", i + 1))
                .collect();
            let outline = crate::tools::outline::format_outline(&syms, line_count);
            let text = format!(
                "[distilled @mention: {} has {line_count} lines ({} bytes); showing top {MENTION_PREVIEW_HEAD_LINES} lines + structural outline — use read(symbols:[...]) or @{}::<symbol> for full bodies]\n\n{head}\n{outline}",
                full.path,
                full.text.len(),
                full.path
            );
            return Ok(MentionFile {
                path: full.path,
                bytes: text.len(),
                text,
                truncated: true,
            });
        }
        if let Some(distilled) = crate::runtime::distill::distill_json_for_model(&full.text) {
            let text = format!(
                "[distilled @mention {}: {} -> {} bytes]\n{}",
                distilled.kind, distilled.original_bytes, distilled.distilled_bytes, distilled.text
            );
            return Ok(MentionFile {
                path: full.path,
                bytes: text.len(),
                text,
                truncated: true,
            });
        }
        if line_count > MENTION_PREVIEW_HEAD_LINES + MENTION_PREVIEW_TAIL_LINES + 20 {
            let lines: Vec<&str> = full.text.lines().collect();
            let omitted = line_count.saturating_sub(
                MENTION_PREVIEW_HEAD_LINES + MENTION_PREVIEW_TAIL_LINES,
            );
            let mut text = format!(
                "[distilled @mention: {} has {line_count} lines; showing head {MENTION_PREVIEW_HEAD_LINES} + tail {MENTION_PREVIEW_TAIL_LINES} — use read(offset, max_lines) for middle]\n",
                full.path
            );
            for (i, line) in lines.iter().take(MENTION_PREVIEW_HEAD_LINES).enumerate() {
                text.push_str(&format!("{}: {line}\n", i + 1));
            }
            text.push_str(&format!("... [{omitted} lines omitted] ...\n"));
            let tail_start = line_count.saturating_sub(MENTION_PREVIEW_TAIL_LINES);
            for (offset, line) in lines[tail_start..].iter().enumerate() {
                text.push_str(&format!("{}: {line}\n", tail_start + offset + 1));
            }
            return Ok(MentionFile {
                path: full.path,
                bytes: text.len(),
                text,
                truncated: true,
            });
        }
        return Ok(full);
    };
    let full = load_mention_file(root, &spec.path, MAX_MENTION_SOURCE_BYTES)?;
    let (display, range) = match selector {
        MentionSelector::LineRange { start, end } => {
            let label = if start == end {
                format!("{}#L{start}", full.path)
            } else {
                format!("{}#L{start}-L{end}", full.path)
            };
            (label, (*start, *end))
        }
        MentionSelector::Symbol(sym) => {
            let syms = crate::tools::outline::extract_outline(Path::new(&full.path), &full.text);
            let ranges = crate::tools::outline::find_symbol_ranges(&syms, &[sym.clone()]);
            let &(start, end) = ranges.first().ok_or_else(|| {
                MentionError::Io(format!("símbolo `{sym}` não encontrado em {}", full.path))
            })?;
            let label = format!("{}::{sym} (L{start}-L{end})", full.path);
            (label, (start, end))
        }
    };
    let lines: Vec<&str> = full.text.lines().collect();
    if range.0 > lines.len() {
        return Err(MentionError::Io(format!(
            "linha inicial {} fora do arquivo ({} linhas)",
            range.0,
            lines.len()
        )));
    }
    let end = range.1.min(lines.len());
    let mut text = String::new();
    let mut truncated = false;
    for idx in range.0..=end {
        let entry = format!("{idx}: {}\n", lines[idx - 1]);
        if !text.is_empty() && text.len() + entry.len() > max_bytes {
            truncated = true;
            break;
        }
        if entry.len() > max_bytes && text.is_empty() {
            let mut cut = max_bytes;
            while cut > 0 && !entry.is_char_boundary(cut) {
                cut -= 1;
            }
            text.push_str(&entry[..cut]);
            truncated = true;
            break;
        }
        text.push_str(&entry);
    }
    if text.ends_with('\n') && !truncated {
        text.pop();
    }
    Ok(MentionFile {
        path: display,
        bytes: text.len(),
        text,
        truncated,
    })
}

fn is_trailing_punctuation(character: char) -> bool {
    matches!(
        character,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
    )
}

/// Resolve `relative` inside the canonical `root` and require a regular file.
/// Returns the canonical path and its workspace-relative display form.
fn resolve_regular_file(root: &Path, relative: &str) -> Result<(PathBuf, String), MentionError> {
    if relative.trim().is_empty() {
        return Err(MentionError::Empty);
    }
    let resolved = resolve_workspace_path_from_root(root, relative).map_err(|message| {
        if message.contains("escapes") || message.contains("absolute") {
            MentionError::OutsideWorkspace
        } else if message.contains("empty") {
            MentionError::Empty
        } else {
            MentionError::Io(message)
        }
    })?;
    let metadata = fs::metadata(&resolved).map_err(map_io_error)?;
    if !metadata.is_file() {
        return Err(MentionError::NotRegularFile);
    }
    let display = relative_slash_path(root, &resolved).ok_or(MentionError::OutsideWorkspace)?;
    Ok((resolved, display))
}

fn relative_slash_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let parts = relative
        .iter()
        .map(|part| part.to_str())
        .collect::<Option<Vec<_>>>()?;
    Some(parts.join("/"))
}

fn map_io_error(error: io::Error) -> MentionError {
    match error.kind() {
        io::ErrorKind::NotFound => MentionError::NotFound,
        _ => MentionError::Io(error.to_string()),
    }
}

#[cfg(windows)]
fn is_reparse_point(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    fs::symlink_metadata(path).map_or(true, |metadata| metadata.file_attributes() & 0x400 != 0)
}

#[cfg(not(windows))]
fn is_reparse_point(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exhausted_time_budget_marks_the_listing_partial() {
        let root = std::env::temp_dir().join(format!("slim-list-budget-{}", std::process::id()));
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("a.txt"), "x").expect("file");
        let cut = list_with_budget(&root, 10, None, Duration::ZERO).expect("list");
        assert!(cut.timed_out);
        let whole = list_with_budget(&root, 10, None, Duration::from_secs(60)).expect("list");
        assert!(!whole.timed_out);
        assert_eq!(whole.files, ["a.txt"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn glob_workspace_files_filters_patterns_and_respects_confinement() {
        let root = std::env::temp_dir().join(format!("slim-glob-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/nested")).expect("dirs");
        fs::create_dir_all(root.join("target/debug")).expect("target");
        fs::write(root.join("src/lib.rs"), "a").expect("file");
        fs::write(root.join("src/nested/mod.rs"), "b").expect("file");
        fs::write(root.join("src/notes.md"), "c").expect("file");
        fs::write(root.join("target/debug/out.rs"), "d").expect("file");
        let canonical = fs::canonicalize(&root).expect("canonical");

        let rs = glob_workspace_files(&canonical, None, Some("**/*.rs"), 10, None).expect("glob");
        assert_eq!(rs.files, vec!["src/lib.rs", "src/nested/mod.rs"]);
        assert!(!rs.truncated);

        let sub = glob_workspace_files(&canonical, Some("src"), Some("*.rs"), 1, None).expect("sub");
        assert_eq!(sub.files, vec!["src/lib.rs"]);
        assert!(sub.truncated);

        assert!(glob_workspace_files(&canonical, None, Some("!*.rs"), 10, None).is_err());
        assert!(glob_workspace_files(&canonical, Some("../outside"), Some("*.rs"), 10, None).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn surgical_line_range_and_symbol_mentions_resolve_and_slice() {
        let root = std::env::temp_dir().join(format!("slim-mention-spec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn alpha() {\n    let a = 1;\n}\n\npub fn beta() -> usize {\n    42\n}\n",
        )
        .unwrap();

        let specs = mention_specs_in_prompt(
            &root,
            "check @src/lib.rs#L1-L3, @src/lib.rs#L6 and @src/lib.rs::beta.",
        );
        assert_eq!(specs.len(), 3);

        let range = load_mention_spec(&root, &specs[0], 4096).expect("range");
        assert_eq!(range.path, "src/lib.rs#L1-L3");
        assert_eq!(range.text, "1: pub fn alpha() {\n2:     let a = 1;\n3: }");

        let single = load_mention_spec(&root, &specs[1], 4096).expect("single");
        assert_eq!(single.path, "src/lib.rs#L6");
        assert_eq!(single.text, "6:     42");

        let sym = load_mention_spec(&root, &specs[2], 4096).expect("symbol");
        assert_eq!(sym.path, "src/lib.rs::beta (L5-L7)");
        assert!(sym.text.contains("5: pub fn beta() -> usize {"));
        assert!(sym.text.contains("6:     42"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_listing_cache_hits_until_invalidated() {
        let root = std::env::temp_dir().join(format!("slim-ws-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("first.txt"), "1").unwrap();
        let canonical = fs::canonicalize(&root).unwrap();
        invalidate_workspace_index(&canonical);

        let first = list_workspace_files(&canonical, 16, None).unwrap();
        assert_eq!(first.files, vec!["first.txt"]);

        fs::write(root.join("second.txt"), "2").unwrap();
        let cached = list_workspace_files(&canonical, 16, None).unwrap();
        let watcher_active = crate::project_index::existing_for_workspace(&canonical)
            .is_some_and(|daemon| daemon.watcher_active());
        if watcher_active {
            // Live FSEvents invalidates the tree on write without waiting for
            // explicit cache invalidation, though async delivery can also land
            // just after this read.
            assert!(
                cached.files == vec!["first.txt"] || cached.files == vec!["first.txt", "second.txt"],
                "unexpected listing under live FSEvents watcher: {:?}",
                cached.files
            );
        } else {
            assert_eq!(cached.files, vec!["first.txt"]);
        }

        invalidate_workspace_index(&canonical);
        let refreshed = list_workspace_files(&canonical, 16, None).unwrap();
        assert_eq!(refreshed.files, vec!["first.txt", "second.txt"]);
        let _ = fs::remove_dir_all(root);
    }
}
