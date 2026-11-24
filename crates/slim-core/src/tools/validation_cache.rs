//! Workspace-digest-keyed Build/Test Validation Receipt Cache (L3/L4).
//!
//! When an allowlisted read-only validation command (`cargo check/test/clippy`,
//! `pytest`, `vitest run`, `jest`, `tsc --noEmit`, `eslint`, `ruff check`,
//! `mypy`, `go test/vet`, etc.) exits cleanly (`exit 0`), we store its redacted
//! receipt in [`crate::cache_db::CacheDb`] keyed by:
//! `(canonical_cwd, normalized_cmd, workspace_tree_digest, env_digest)`.
//!
//! If another agent (e.g. Swarm `@verifier` after `@dev`) runs the exact same
//! validation while `workspace_tree_digest` is unchanged (checked in O(1) via
//! [`crate::project_index::ProjectIndexDaemon`]), the verified receipt returns
//! in 0 ms while preserving `"exit 0"` verbatim on line 1 (required by
//! `CausalGovernor::validation_green`).
//!
//! Bypass: pass `no_cache: true` to the `shell` tool or set
//! `SLIM_VALIDATION_CACHE=0`.

use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::cache_db::{self, CacheDb, DEFAULT_VALIDATION_TTL_SECS};

const RELEVANT_ENV_VARS: &[&str] = &[
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTUP_TOOLCHAIN",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET",
    "CARGO_PROFILE",
    "NODE_ENV",
    "PYTEST_ADDOPTS",
    "PYTHONPATH",
    "VIRTUAL_ENV",
    "GOPATH",
    "GOROOT",
    "GOOS",
    "GOARCH",
    "CI",
    "TARGET",
];

const SHELL_METACHARS: &[char] = &[';', '|', '&', '>', '<', '`', '$', '\n', '\r'];

const DISALLOWED_FLAGS: &[&str] = &[
    "--fix",
    "--write",
    "-u",
    "--update-snapshots",
    "--overwrite",
    "--help",
    "-h",
    "--version",
    "-V",
    "--watch",
    "-w",
];

pub(crate) fn validation_cache_enabled() -> bool {
    !std::env::var("SLIM_VALIDATION_CACHE")
        .ok()
        .is_some_and(|v| matches!(v.trim(), "0" | "false" | "FALSE" | "off" | "OFF"))
}

/// Returns a canonical normalized command string if `(command, args)` is an
/// allowlisted, side-effect-free validation command eligible for receipt caching.
pub(crate) fn normalized_cacheable_validation(
    command: &str,
    args: Option<&[String]>,
) -> Option<String> {
    let tokens: Vec<&str> = match args {
        Some(list) => {
            let mut v = Vec::with_capacity(1 + list.len());
            v.push(command);
            for a in list {
                v.push(a.as_str());
            }
            v
        }
        None => {
            if command.chars().any(|c| SHELL_METACHARS.contains(&c)) {
                return None;
            }
            command.split_whitespace().collect()
        }
    };
    if tokens.is_empty() {
        return None;
    }
    if tokens.iter().any(|tok| {
        let unquoted = tok
            .strip_prefix('\'')
            .and_then(|t| t.strip_suffix('\''))
            .or_else(|| tok.strip_prefix('"').and_then(|t| t.strip_suffix('"')))
            .unwrap_or(tok);
        DISALLOWED_FLAGS.contains(&unquoted) || unquoted.starts_with("--fix=")
    }) {
        return None;
    }

    let mut idx = 0usize;
    if tokens.first() == Some(&"env") {
        idx = 1;
        while idx < tokens.len() {
            if tokens[idx] == "-u" {
                idx = idx.saturating_add(2);
            } else if tokens[idx].starts_with("--unset=") {
                idx = idx.saturating_add(1);
            } else {
                break;
            }
        }
    }

    let bin = *tokens.get(idx)?;
    let rest = &tokens[idx + 1..];
    let allowed = match bin {
        "cargo" | "cargo.exe" => match rest.first().copied() {
            Some("test" | "check" | "clippy") => true,
            Some("fmt") => rest[1..].contains(&"--check"),
            _ => false,
        },
        "pytest" | "py.test" | "mypy" => true,
        "ruff" => matches!(rest.first().copied(), Some("check")),
        "vitest" => matches!(rest.first().copied(), Some("run")),
        "jest" => !rest.contains(&"--watchAll"),
        "tsc" => rest.contains(&"--noEmit"),
        "eslint" => true,
        "go" => matches!(rest.first().copied(), Some("test" | "vet")),
        "npm" | "pnpm" | "yarn" | "bun" => match rest.first().copied() {
            Some("test") => true,
            Some("run") => matches!(
                rest.get(1).copied(),
                Some("test" | "check" | "lint" | "typecheck" | "type-check")
            ),
            _ => false,
        },
        _ => false,
    };
    if !allowed {
        return None;
    }
    Some(tokens.join(" "))
}

pub(crate) fn compute_env_digest(bin: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"slim-validation-env-v1\n");
    hasher.update(std::env::consts::OS.as_bytes());
    hasher.update(b"/");
    hasher.update(std::env::consts::ARCH.as_bytes());
    hasher.update(b"\nbin:");
    hasher.update(bin.as_bytes());
    hasher.update(b"\n");
    for &var in RELEVANT_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            hasher.update(var.as_bytes());
            hasher.update(b"=");
            hasher.update(val.as_bytes());
            hasher.update([0u8]);
        }
    }
    hasher.finalize().into()
}

fn make_receipt_key(canonical_cwd: &Path, normalized_cmd: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"slim-validation-key-v1\n");
    hasher.update(canonical_cwd.to_string_lossy().as_bytes());
    hasher.update([0u8]);
    hasher.update(normalized_cmd.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

#[derive(Clone, Debug)]
pub(crate) struct CachedValidationHit {
    pub(crate) output: String,
}

/// Attempts to resolve a verified green validation receipt from `db` (or the
/// shared L4 SQLite DB) when the workspace tree digest matches in O(1).
pub(crate) fn try_lookup_in(
    db: &Arc<CacheDb>,
    canonical_cwd: &Path,
    command: &str,
    args: Option<&[String]>,
) -> Option<CachedValidationHit> {
    if !validation_cache_enabled() {
        return None;
    }
    let norm = normalized_cacheable_validation(command, args)?;
    let daemon = crate::project_index::for_workspace(canonical_cwd).ok()?;
    let tree_digest = daemon.workspace_tree_digest()?;
    let bin = norm.split_whitespace().next().unwrap_or(command);
    let env_digest = compute_env_digest(bin);
    let key = make_receipt_key(canonical_cwd, &norm);
    match db.get_validation_receipt(&key, &tree_digest, &env_digest) {
        Some(receipt) if receipt.exit_code == 0 => {
            cache_db::record_validation_hit();
            let output = format_cached_output(&receipt.stdout, receipt.duration_ms);
            Some(CachedValidationHit { output })
        }
        _ => {
            cache_db::record_validation_miss();
            None
        }
    }
}

pub(crate) fn try_lookup(
    canonical_cwd: &Path,
    command: &str,
    args: Option<&[String]>,
) -> Option<CachedValidationHit> {
    let db = CacheDb::default_shared()?;
    try_lookup_in(&db, canonical_cwd, command, args)
}

pub(crate) fn record_success_in(
    db: &Arc<CacheDb>,
    canonical_cwd: &Path,
    command: &str,
    args: Option<&[String]>,
    redacted_output: &str,
    duration_ms: u64,
) {
    if !validation_cache_enabled() {
        return;
    }
    let Some(norm) = normalized_cacheable_validation(command, args) else {
        return;
    };
    let Ok(daemon) = crate::project_index::for_workspace(canonical_cwd) else {
        return;
    };
    let Some(tree_digest) = daemon.workspace_tree_digest() else {
        return;
    };
    let bin = norm.split_whitespace().next().unwrap_or(command);
    let env_digest = compute_env_digest(bin);
    let key = make_receipt_key(canonical_cwd, &norm);
    db.put_validation_receipt(
        &key,
        &canonical_cwd.to_string_lossy(),
        &norm,
        &tree_digest,
        &env_digest,
        0,
        redacted_output,
        "",
        duration_ms,
        DEFAULT_VALIDATION_TTL_SECS,
    );
}

pub(crate) fn record_success(
    canonical_cwd: &Path,
    command: &str,
    args: Option<&[String]>,
    redacted_output: &str,
    duration_ms: u64,
) {
    let Some(db) = CacheDb::default_shared() else {
        return;
    };
    record_success_in(&db, canonical_cwd, command, args, redacted_output, duration_ms);
}

/// Keeps `"exit 0"` verbatim on line 1 (contract of `CausalGovernor::validation_green`)
/// and inserts the verified-cache tag on line 2.
fn format_cached_output(stored: &str, saved_ms: u64) -> String {
    let tag = format!("[verified-cache hit · 0ms · saved {saved_ms}ms · workspace unchanged]");
    match stored.split_once('\n') {
        Some((first, rest)) if first == "exit 0" => format!("{first}\n{tag}\n{rest}"),
        _ => format!("exit 0\n{tag}\n{stored}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn allowlist_accepts_safe_validations_and_rejects_mutating_or_piped_commands() {
        assert!(normalized_cacheable_validation("cargo check --workspace", None).is_some());
        assert!(normalized_cacheable_validation("cargo fmt --check", None).is_some());
        assert!(normalized_cacheable_validation("pytest -q", None).is_some());
        assert!(normalized_cacheable_validation("vitest run", None).is_some());
        assert!(normalized_cacheable_validation("tsc --noEmit", None).is_some());
        assert!(normalized_cacheable_validation("eslint src/", None).is_some());
        assert!(normalized_cacheable_validation("go test ./...", None).is_some());

        // Unsafe / mutating commands must never be cached.
        assert!(normalized_cacheable_validation("cargo fmt", None).is_none());
        assert!(normalized_cacheable_validation("eslint --fix src/", None).is_none());
        assert!(normalized_cacheable_validation("pytest | tee log.txt", None).is_none());
        assert!(normalized_cacheable_validation("cargo test && rm -rf /tmp/x", None).is_none());
        assert!(normalized_cacheable_validation("vitest --watch", None).is_none());
    }

    #[test]
    fn validation_receipt_hits_when_unchanged_and_misses_after_one_byte_edit() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "slim-val-cache-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lib.rs");
        fs::write(&file, "pub fn a() -> i32 { 1 }\n").unwrap();
        let canon = fs::canonicalize(&dir).unwrap();
        let canon_file = fs::canonicalize(&file).unwrap();

        let db = Arc::new(CacheDb::open_in_memory().expect("mem db"));
        assert!(try_lookup_in(&db, &canon, "cargo check", None).is_none());

        record_success_in(
            &db,
            &canon,
            "cargo check",
            None,
            "exit 0\nstdout:\nFinished dev profile\nstderr:\n",
            450,
        );

        let hit = try_lookup_in(&db, &canon, "cargo check", None).expect("must hit");
        assert_eq!(
            hit.output.lines().next(),
            Some("exit 0"),
            "line 1 must remain verbatim `exit 0` for CausalGovernor::validation_green"
        );
        assert!(hit.output.contains("[verified-cache hit · 0ms · saved 450ms · workspace unchanged]"));

        // Edit a single byte and notify daemon -> must miss immediately.
        fs::write(&canon_file, "pub fn a() -> i32 { 2 }\n").unwrap();
        if let Ok(daemon) = crate::project_index::for_workspace(&canon) {
            daemon.notify_local_write(&canon_file, Some(b"pub fn a() -> i32 { 2 }\n"));
        }
        assert!(
            try_lookup_in(&db, &canon, "cargo check", None).is_none(),
            "changing 1 byte must invalidate the validation receipt"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
