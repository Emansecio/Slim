//! Workspace file discovery and confined `@mention` loading for the TUI.
//!
//! Both entry points stay inside the canonical workspace root: listing never
//! follows links, and loading resolves through the same confinement helper the
//! native tools use, so `..`, absolute paths and links leaving the workspace
//! are rejected.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;

use crate::runtime::CancellationToken;
use crate::tools::{resolve_workspace_path_from_root, SKIP_DIR_NAMES};

/// Listing is a completion aid; it must never block the caller for long.
const LIST_TIME_BUDGET: Duration = Duration::from_millis(1500);
/// A NUL byte in this prefix marks the file as binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Candidate cap per prompt so a pasted wall of `@` tokens cannot stat-storm.
const MAX_MENTION_CANDIDATES: usize = 64;
const MAX_MENTION_TOKEN_BYTES: usize = 1024;

/// Workspace-relative, forward-slash paths of regular files honoring
/// `.gitignore`, skipping links, Windows junctions and the directories in
/// `SKIP_DIR_NAMES`. Dotfiles are included. The result is bounded by `limit`
/// and a time budget (partial output is returned when either is hit), sorted
/// shallower-first then lexicographically. Cancellation yields
/// `ErrorKind::Interrupted`.
pub fn list_workspace_files(
    root: &Path,
    limit: usize,
    cancel: Option<&CancellationToken>,
) -> io::Result<Vec<String>> {
    let started = Instant::now();
    let cancelled = || cancel.is_some_and(CancellationToken::is_cancelled);
    let root = fs::canonicalize(root)?;
    if limit == 0 {
        return Ok(Vec::new());
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
        true
    });

    let mut files = Vec::new();
    for entry in builder.build() {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace listing cancelled",
            ));
        }
        if started.elapsed() >= LIST_TIME_BUDGET || files.len() >= limit {
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
    Ok(files)
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
