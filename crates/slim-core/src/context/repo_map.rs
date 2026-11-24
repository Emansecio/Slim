//! Compact, deterministic workspace symbol map (`RepoMap`) injected into the
//! initial workspace snapshot and refreshed on compaction.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::runtime::CancellationToken;
use crate::tools::outline::extract_outline;
use crate::tools::SKIP_DIR_NAMES;

pub const DEFAULT_REPO_MAP_BYTES: usize = 1_800;
const MAX_SCAN_FILES: usize = 512;
const MAX_SELECTED_FILES: usize = 24;
const MAX_FILE_READ_BYTES: u64 = 256 * 1024;

fn is_source_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "rs" | "ts" | "tsx" | "mts" | "js" | "jsx" | "mjs" | "py" | "go" | "c" | "h" | "cc"
            | "cpp" | "hpp" | "java" | "swift" | "kt"
    )
}

fn is_excluded_rel(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.contains("/test/")
        || lower.starts_with("test/")
        || lower.contains("/fixtures/")
        || lower.contains("/benches/")
        || lower.contains("_test.")
        || lower.contains("_tests.")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".test.js")
        || lower.ends_with(".spec.ts")
}

fn file_priority(rel: &str) -> (u8, usize, &str) {
    let file = Path::new(rel)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let entrypoint = matches!(
        file,
        "lib.rs" | "main.rs" | "mod.rs" | "index.ts" | "index.js" | "__init__.py" | "main.go"
    );
    let core_dir = rel.starts_with("src/")
        || rel.starts_with("crates/")
        || rel.starts_with("app/src/")
        || rel.starts_with("lib/")
        || rel.starts_with("pkg/");
    let tier = match (core_dir, entrypoint) {
        (true, true) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (false, false) => 3,
    };
    let depth = rel.bytes().filter(|&b| b == b'/').count();
    (tier, depth, rel)
}

/// Generates a deterministic compact symbol map (`path: sym1, sym2, ...`) bounded by `max_bytes`.
pub fn build_repo_map(
    root: &Path,
    max_bytes: usize,
    cancellation: Option<&CancellationToken>,
) -> Option<String> {
    if max_bytes < 64 || cancellation.is_some_and(CancellationToken::is_cancelled) {
        return None;
    }
    let canonical = fs::canonicalize(root).ok()?;
    let mut builder = WalkBuilder::new(&canonical);
    builder
        .max_depth(Some(4))
        .require_git(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .hidden(true)
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(|entry| {
            entry.depth() == 0
                || (!entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| SKIP_DIR_NAMES.contains(&name))
                    && entry.file_type().is_some_and(|kind| !kind.is_symlink()))
        });

    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for entry in builder.build() {
        if cancellation.is_some_and(CancellationToken::is_cancelled)
            || candidates.len() >= MAX_SCAN_FILES
        {
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if !is_source_extension(entry.path()) {
            continue;
        }
        let Ok(rel_path) = entry.path().strip_prefix(&canonical) else {
            continue;
        };
        let Some(parts) = rel_path
            .iter()
            .map(|p| p.to_str())
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let rel = parts.join("/");
        if is_excluded_rel(&rel) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.len() == 0 || meta.len() > MAX_FILE_READ_BYTES {
            continue;
        }
        candidates.push((rel, entry.path().to_path_buf()));
    }
    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by(|a, b| file_priority(&a.0).cmp(&file_priority(&b.0)));
    candidates.truncate(MAX_SELECTED_FILES);
    // Re-sort deterministically by relative path for stable prompt cache ordering.
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let daemon = crate::project_index::for_workspace(&canonical).ok();
    let mut entries: Vec<(String, String)> = Vec::new();
    for (rel, path) in candidates {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            break;
        }
        let cached_syms = daemon
            .as_ref()
            .and_then(|d| d.read_file_cached(&path).ok().flatten())
            .map(|file| file.outline());
        let syms_owned;
        let syms: &[crate::tools::outline::OutlineSymbol] = match &cached_syms {
            Some(arc) => arc.as_ref(),
            None => {
                let Ok(content) = fs::read_to_string(&path) else {
                    continue;
                };
                syms_owned = extract_outline(&path, &content);
                &syms_owned
            }
        };
        if syms.is_empty() {
            continue;
        }
        let mut names = Vec::new();
        for sym in syms {
            if !matches!(
                sym.kind,
                "fn" | "struct" | "enum" | "trait" | "class" | "interface" | "type" | "namespace"
            ) {
                continue;
            }
            if !names.contains(&sym.name) {
                names.push(sym.name.clone());
            }
            if names.len() >= 12 {
                break;
            }
        }
        if !names.is_empty() {
            entries.push((rel, names.join(", ")));
        }
    }

    if entries.is_empty() {
        return None;
    }

    let mut out = String::new();
    for (rel, syms) in entries {
        let line = format!("- {rel}: {syms}");
        if out.len() + line.len() + 1 > max_bytes {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = write!(out, "{line}");
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_deterministic_symbol_map_excluding_tests() {
        let root = std::env::temp_dir().join(format!("slim-repo-map-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct Engine {}\npub fn start_engine() {}\n",
        )
        .unwrap();
        fs::write(root.join("tests/it.rs"), "fn test_only() {}\n").unwrap();

        let map = build_repo_map(&root, DEFAULT_REPO_MAP_BYTES, None).expect("map");
        assert!(map.contains("- src/lib.rs: Engine, start_engine"));
        assert!(!map.contains("test_only"));
        let _ = fs::remove_dir_all(root);
    }
}
