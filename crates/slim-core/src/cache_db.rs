//! Unified persistent L4 cache backed by SQLite WAL (`~/.slim/cache/l4-cache.sqlite`).
//!
//! Stores three strictly bounded, TTL-governed tables:
//! 1. `provider_responses`: deterministic non-tool LLM responses (titles, commit
//!    drafts, reviews, single-turn completions) keyed by SHA-256 v5 keys with
//!    stable tenant fingerprints (never raw credentials, never persisted under ZDR).
//! 2. `validation_receipts`: verified green build/test receipts keyed by
//!    `(canonical_cwd, normalized_cmd, workspace_tree_digest, env_digest)` so
//!    `@verifier` or repeat checks skip redundant runs when zero files changed.
//! 3. `http_catalogs`: remote model catalogs with `ETag` / `If-None-Match`
//!    revalidation (`304 Not Modified`).
//!
//! Kill-switch: `SLIM_L4_CACHE=0` disables disk persistence globally.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS provider_responses (
    cache_key TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    last_hit_at INTEGER NOT NULL,
    ttl_secs INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_provider_responses_hit ON provider_responses(last_hit_at);

CREATE TABLE IF NOT EXISTS validation_receipts (
    receipt_key TEXT PRIMARY KEY,
    canonical_cwd TEXT NOT NULL,
    command_norm TEXT NOT NULL,
    tree_digest BLOB NOT NULL,
    env_digest BLOB NOT NULL,
    exit_code INTEGER NOT NULL,
    stdout TEXT NOT NULL,
    stderr TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    ttl_secs INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_validation_receipts_created ON validation_receipts(created_at);

CREATE TABLE IF NOT EXISTS http_catalogs (
    url TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    etag TEXT,
    body BLOB NOT NULL,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (url, scope_key)
);
"#;

const MAX_PROVIDER_CACHE_BYTES: i64 = 64 * 1024 * 1024;
const MAX_VALIDATION_CACHE_BYTES: i64 = 32 * 1024 * 1024;
pub const DEFAULT_PROVIDER_TTL_SECS: u64 = 24 * 3600;
pub const DEFAULT_VALIDATION_TTL_SECS: u64 = 600;

static PROVIDER_L4_HITS: AtomicU64 = AtomicU64::new(0);
static PROVIDER_L4_MISSES: AtomicU64 = AtomicU64::new(0);
static VALIDATION_HITS: AtomicU64 = AtomicU64::new(0);
static VALIDATION_MISSES: AtomicU64 = AtomicU64::new(0);
static CATALOG_304_HITS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheDbStats {
    pub provider_l4_hits: u64,
    pub provider_l4_misses: u64,
    pub validation_hits: u64,
    pub validation_misses: u64,
    pub catalog_304_hits: u64,
}

pub fn global_stats() -> CacheDbStats {
    CacheDbStats {
        provider_l4_hits: PROVIDER_L4_HITS.load(Ordering::Relaxed),
        provider_l4_misses: PROVIDER_L4_MISSES.load(Ordering::Relaxed),
        validation_hits: VALIDATION_HITS.load(Ordering::Relaxed),
        validation_misses: VALIDATION_MISSES.load(Ordering::Relaxed),
        catalog_304_hits: CATALOG_304_HITS.load(Ordering::Relaxed),
    }
}

pub fn record_validation_hit() {
    VALIDATION_HITS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_validation_miss() {
    VALIDATION_MISSES.fetch_add(1, Ordering::Relaxed);
}

pub fn record_catalog_304_hit() {
    CATALOG_304_HITS.fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredValidationReceipt {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub created_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredCatalog {
    pub etag: Option<String>,
    pub body: Vec<u8>,
    pub fetched_at: u64,
}

/// Thread-safe handle to the SQLite WAL L4 cache database.
pub struct CacheDb {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for CacheDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheDb").finish_non_exhaustive()
    }
}

static DEFAULT_DB: OnceLock<Option<Arc<CacheDb>>> = OnceLock::new();

fn env_disabled() -> bool {
    std::env::var("SLIM_L4_CACHE")
        .ok()
        .is_some_and(|v| matches!(v.trim(), "0" | "false" | "FALSE" | "off" | "OFF"))
}

pub fn default_cache_db_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("SLIM_L4_CACHE_PATH") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    if let Some(slim_home) = std::env::var_os("SLIM_HOME") {
        if !slim_home.is_empty() {
            return Some(PathBuf::from(slim_home).join("cache").join("l4-cache.sqlite"));
        }
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".slim")
            .join("cache")
            .join("l4-cache.sqlite"),
    )
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl CacheDb {
    /// Returns the shared process-wide L4 database, or `None` when disabled by
    /// `SLIM_L4_CACHE=0` or when disk initialization fails (fail-open).
    pub fn default_shared() -> Option<Arc<Self>> {
        if env_disabled() {
            return None;
        }
        DEFAULT_DB
            .get_or_init(|| {
                let path = default_cache_db_path()?;
                Self::open(&path).ok().map(Arc::new)
            })
            .clone()
    }

    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create cache dir: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        let conn = Connection::open(path).map_err(|e| format!("open cache db: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        configure_connection(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| format!("open memory db: {e}"))?;
        configure_connection(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- 1. Provider responses -------------------------------------------

    pub fn get_provider_response(&self, cache_key: &str) -> Option<String> {
        let now = now_unix_secs() as i64;
        let conn = self.lock_conn();
        let row: Option<(String, i64, i64)> = conn
            .query_row(
                "SELECT payload_json, created_at, ttl_secs FROM provider_responses WHERE cache_key = ?1",
                params![cache_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()
            .flatten();
        let Some((payload, created_at, ttl_secs)) = row else {
            PROVIDER_L4_MISSES.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if now.saturating_sub(created_at) > ttl_secs {
            let _ = conn.execute(
                "DELETE FROM provider_responses WHERE cache_key = ?1",
                params![cache_key],
            );
            PROVIDER_L4_MISSES.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let _ = conn.execute(
            "UPDATE provider_responses SET last_hit_at = ?1 WHERE cache_key = ?2",
            params![now, cache_key],
        );
        PROVIDER_L4_HITS.fetch_add(1, Ordering::Relaxed);
        Some(payload)
    }

    pub fn put_provider_response(
        &self,
        cache_key: &str,
        provider: &str,
        model: &str,
        payload_json: &str,
        ttl_secs: u64,
    ) {
        let size = payload_json.len() as i64;
        if size <= 0 || size > 2 * 1024 * 1024 {
            return;
        }
        let now = now_unix_secs() as i64;
        let conn = self.lock_conn();
        let _ = conn.execute(
            "INSERT INTO provider_responses(cache_key, provider, model, payload_json, size_bytes, created_at, last_hit_at, ttl_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)
             ON CONFLICT(cache_key) DO UPDATE SET
               payload_json = excluded.payload_json,
               size_bytes = excluded.size_bytes,
               created_at = excluded.created_at,
               last_hit_at = excluded.last_hit_at,
               ttl_secs = excluded.ttl_secs",
            params![cache_key, provider, model, payload_json, size, now, ttl_secs as i64],
        );
        prune_provider_table(&conn, now);
    }

    pub fn delete_provider_response(&self, cache_key: &str) {
        let conn = self.lock_conn();
        let _ = conn.execute(
            "DELETE FROM provider_responses WHERE cache_key = ?1",
            params![cache_key],
        );
    }

    pub fn clear_provider_responses(&self) {
        let conn = self.lock_conn();
        let _ = conn.execute("DELETE FROM provider_responses", []);
    }

    // ---- 2. Validation receipts ------------------------------------------

    pub fn get_validation_receipt(
        &self,
        receipt_key: &str,
        tree_digest: &[u8; 32],
        env_digest: &[u8; 32],
    ) -> Option<StoredValidationReceipt> {
        let now = now_unix_secs() as i64;
        let conn = self.lock_conn();
        let row: Option<(Vec<u8>, Vec<u8>, i32, String, String, i64, i64, i64)> = conn
            .query_row(
                "SELECT tree_digest, env_digest, exit_code, stdout, stderr, duration_ms, created_at, ttl_secs
                 FROM validation_receipts WHERE receipt_key = ?1",
                params![receipt_key],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()
            .ok()
            .flatten();
        let (td, ed, exit_code, stdout, stderr, duration_ms, created_at, ttl_secs) = row?;
        if td.as_slice() != tree_digest
            || ed.as_slice() != env_digest
            || now.saturating_sub(created_at) > ttl_secs
        {
            return None;
        }
        Some(StoredValidationReceipt {
            exit_code,
            stdout,
            stderr,
            duration_ms: duration_ms.max(0) as u64,
            created_at: created_at.max(0) as u64,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn put_validation_receipt(
        &self,
        receipt_key: &str,
        canonical_cwd: &str,
        command_norm: &str,
        tree_digest: &[u8; 32],
        env_digest: &[u8; 32],
        exit_code: i32,
        stdout: &str,
        stderr: &str,
        duration_ms: u64,
        ttl_secs: u64,
    ) {
        if stdout.len() + stderr.len() > 512 * 1024 {
            return;
        }
        let now = now_unix_secs() as i64;
        let conn = self.lock_conn();
        let _ = conn.execute(
            "INSERT INTO validation_receipts(receipt_key, canonical_cwd, command_norm, tree_digest, env_digest, exit_code, stdout, stderr, duration_ms, created_at, ttl_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(receipt_key) DO UPDATE SET
               tree_digest = excluded.tree_digest,
               env_digest = excluded.env_digest,
               exit_code = excluded.exit_code,
               stdout = excluded.stdout,
               stderr = excluded.stderr,
               duration_ms = excluded.duration_ms,
               created_at = excluded.created_at,
               ttl_secs = excluded.ttl_secs",
            params![
                receipt_key,
                canonical_cwd,
                command_norm,
                tree_digest.as_slice(),
                env_digest.as_slice(),
                exit_code,
                stdout,
                stderr,
                duration_ms as i64,
                now,
                ttl_secs as i64,
            ],
        );
        prune_validation_table(&conn, now);
    }

    // ---- 3. HTTP catalogs (ETag / If-None-Match) -------------------------

    pub fn get_catalog(&self, url: &str, scope_key: &str) -> Option<StoredCatalog> {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT etag, body, fetched_at FROM http_catalogs WHERE url = ?1 AND scope_key = ?2",
            params![url, scope_key],
            |r| {
                Ok(StoredCatalog {
                    etag: r.get(0)?,
                    body: r.get(1)?,
                    fetched_at: r.get::<_, i64>(2)?.max(0) as u64,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn put_catalog(&self, url: &str, scope_key: &str, etag: Option<&str>, body: &[u8]) {
        let now = now_unix_secs() as i64;
        let conn = self.lock_conn();
        let _ = conn.execute(
            "INSERT INTO http_catalogs(url, scope_key, etag, body, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(url, scope_key) DO UPDATE SET
               etag = excluded.etag,
               body = excluded.body,
               fetched_at = excluded.fetched_at",
            params![url, scope_key, etag, body, now],
        );
    }

    pub fn touch_catalog(&self, url: &str, scope_key: &str, etag: Option<&str>) {
        let now = now_unix_secs() as i64;
        let conn = self.lock_conn();
        if let Some(tag) = etag {
            let _ = conn.execute(
                "UPDATE http_catalogs SET fetched_at = ?1, etag = ?2 WHERE url = ?3 AND scope_key = ?4",
                params![now, tag, url, scope_key],
            );
        } else {
            let _ = conn.execute(
                "UPDATE http_catalogs SET fetched_at = ?1 WHERE url = ?2 AND scope_key = ?3",
                params![now, url, scope_key],
            );
        }
    }

    pub fn delete_catalog(&self, url: &str, scope_key: Option<&str>) {
        let conn = self.lock_conn();
        match scope_key {
            Some(scope) => {
                let _ = conn.execute(
                    "DELETE FROM http_catalogs WHERE url = ?1 AND scope_key = ?2",
                    params![url, scope],
                );
            }
            None => {
                let _ = conn.execute("DELETE FROM http_catalogs WHERE url = ?1", params![url]);
            }
        }
    }
}

fn configure_connection(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA temp_store = MEMORY;",
    )
    .map_err(|e| format!("configure cache pragmas: {e}"))?;
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| format!("init cache schema: {e}"))?;
    Ok(())
}

fn prune_provider_table(conn: &Connection, now: i64) {
    let _ = conn.execute(
        "DELETE FROM provider_responses WHERE (?1 - created_at) > ttl_secs",
        params![now],
    );
    let total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM provider_responses",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if total > MAX_PROVIDER_CACHE_BYTES {
        let _ = conn.execute(
            "DELETE FROM provider_responses WHERE cache_key IN (
                SELECT cache_key FROM provider_responses ORDER BY last_hit_at ASC LIMIT 64
             )",
            [],
        );
    }
}

fn prune_validation_table(conn: &Connection, now: i64) {
    let _ = conn.execute(
        "DELETE FROM validation_receipts WHERE (?1 - created_at) > ttl_secs",
        params![now],
    );
    let total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(LENGTH(stdout) + LENGTH(stderr)), 0) FROM validation_receipts",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if total > MAX_VALIDATION_CACHE_BYTES {
        let _ = conn.execute(
            "DELETE FROM validation_receipts WHERE receipt_key IN (
                SELECT receipt_key FROM validation_receipts ORDER BY created_at ASC LIMIT 32
             )",
            [],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_wal_persists_across_reopen_for_responses_validations_and_catalogs() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "slim-cache-db-test-{}-{nanos}.sqlite",
            std::process::id()
        ));

        {
            let db = CacheDb::open(&path).expect("open db");
            db.put_provider_response("k1", "anthropic", "claude-haiku", r#"["ok"]"#, 3600);
            db.put_validation_receipt(
                "v1",
                "/repo",
                "cargo check",
                &[7u8; 32],
                &[3u8; 32],
                0,
                "exit 0\nstdout:\n",
                "",
                120,
                600,
            );
            db.put_catalog("https://api.anthropic.com/v1/models", "acct-1", Some("\"etag-1\""), b"{\"data\":[]}");
        }

        // Reopen simulates a fresh process restart.
        {
            let db = CacheDb::open(&path).expect("reopen db");
            assert_eq!(db.get_provider_response("k1").as_deref(), Some(r#"["ok"]"#));
            let receipt = db
                .get_validation_receipt("v1", &[7u8; 32], &[3u8; 32])
                .expect("receipt hit");
            assert_eq!(receipt.exit_code, 0);
            assert!(
                db.get_validation_receipt("v1", &[8u8; 32], &[3u8; 32]).is_none(),
                "different tree digest must miss"
            );
            let cat = db
                .get_catalog("https://api.anthropic.com/v1/models", "acct-1")
                .expect("catalog hit");
            assert_eq!(cat.etag.as_deref(), Some("\"etag-1\""));
            db.delete_catalog("https://api.anthropic.com/v1/models", Some("acct-1"));
            assert!(db.get_catalog("https://api.anthropic.com/v1/models", "acct-1").is_none());
        }

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = fs::remove_file(path.with_extension("sqlite-shm"));
    }
}
