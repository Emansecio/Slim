use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use slim_core::provider::{
    codex_models_url, parse_codex_catalog, CodexCatalogEntry, CodexCatalogError as CoreCatalogError,
};

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexCatalogError {
    Empty,
    InvalidSchema,
    DuplicateSlug,
    Oversized,
    Network,
    UnsafeCache,
    CacheIo,
}

impl From<CoreCatalogError> for CodexCatalogError {
    fn from(error: CoreCatalogError) -> Self {
        match error {
            CoreCatalogError::InvalidSchema => Self::InvalidSchema,
            CoreCatalogError::DuplicateSlug => Self::DuplicateSlug,
            CoreCatalogError::Oversized => Self::Oversized,
        }
    }
}

impl fmt::Display for CodexCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "Codex catalog contained no models",
            Self::InvalidSchema => "Codex catalog schema is invalid",
            Self::DuplicateSlug => "Codex catalog contains a duplicate model slug",
            Self::Oversized => "Codex catalog exceeds its bound",
            Self::Network => "Codex catalog request failed",
            Self::UnsafeCache => "Codex catalog cache path is unsafe",
            Self::CacheIo => "Codex catalog cache I/O failed",
        })
    }
}

impl std::error::Error for CodexCatalogError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexCatalogSnapshot {
    pub entries: Vec<CodexCatalogEntry>,
}

struct MemoryCache {
    key: String,
    fetched_at: Instant,
    entries: Vec<CodexCatalogEntry>,
}

static MEMORY_CACHE: Mutex<Option<MemoryCache>> = Mutex::new(None);
static LAST_REFRESH: Mutex<Option<(String, Instant)>> = Mutex::new(None);

#[derive(Clone)]
pub struct CodexCatalog {
    cache_path: PathBuf,
    client: reqwest::Client,
}

impl CodexCatalog {
    pub fn production() -> Result<Self, CodexCatalogError> {
        let cache_path = if let Some(path) = std::env::var_os("SLIM_CODEX_MODELS_FILE") {
            if path.is_empty() {
                return Err(CodexCatalogError::UnsafeCache);
            }
            PathBuf::from(path)
        } else {
            let profile = std::env::var_os("USERPROFILE").ok_or(CodexCatalogError::UnsafeCache)?;
            if profile.is_empty() {
                return Err(CodexCatalogError::UnsafeCache);
            }
            PathBuf::from(profile)
                .join(".slim")
                .join("codex-models.json")
        };
        Self::at(cache_path)
    }

    pub fn at(cache_path: impl Into<PathBuf>) -> Result<Self, CodexCatalogError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| CodexCatalogError::Network)?;
        Ok(Self {
            cache_path: cache_path.into(),
            client,
        })
    }

    pub async fn load(
        &self,
        endpoint: &str,
        access_token: &str,
        account_id: &str,
    ) -> Result<CodexCatalogSnapshot, CodexCatalogError> {
        if let Some(snapshot) = self.load_cached(endpoint, account_id)? {
            return Ok(snapshot);
        }
        fetch_with_client(
            endpoint,
            access_token,
            account_id,
            &self.cache_path,
            &self.client,
        )
        .await
    }

    pub fn load_cached(
        &self,
        endpoint: &str,
        account_id: &str,
    ) -> Result<Option<CodexCatalogSnapshot>, CodexCatalogError> {
        let key = catalog_key(endpoint, account_id);
        if let Ok(guard) = MEMORY_CACHE.lock() {
            if let Some(cache) = guard.as_ref() {
                if cache.key == key && cache.fetched_at.elapsed() < CACHE_TTL {
                    return Ok(Some(CodexCatalogSnapshot {
                        entries: cache.entries.clone(),
                    }));
                }
            }
        }
        let Some(entries) = read_cache(&self.cache_path)? else {
            return Ok(None);
        };
        if let Ok(mut guard) = MEMORY_CACHE.lock() {
            *guard = Some(MemoryCache {
                key,
                fetched_at: Instant::now(),
                entries: entries.clone(),
            });
        }
        Ok(Some(CodexCatalogSnapshot { entries }))
    }

    pub fn refresh_in_background(&self, endpoint: &str, access_token: &str, account_id: &str) {
        let key = catalog_key(endpoint, account_id);
        let should_refresh = LAST_REFRESH.lock().is_ok_and(|mut guard| {
            if guard
                .as_ref()
                .is_some_and(|(cached_key, at)| cached_key == &key && at.elapsed() < CACHE_TTL)
            {
                false
            } else {
                *guard = Some((key, Instant::now()));
                true
            }
        });
        if !should_refresh {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let endpoint = endpoint.to_owned();
        let access_token = access_token.to_owned();
        let account_id = account_id.to_owned();
        let cache_path = self.cache_path.clone();
        let client = self.client.clone();
        handle.spawn(async move {
            let _ = fetch_with_client(&endpoint, &access_token, &account_id, &cache_path, &client)
                .await;
        });
    }
}

fn catalog_key(endpoint: &str, account_id: &str) -> String {
    format!("{endpoint}\0{account_id}")
}

fn read_cache(path: &Path) -> Result<Option<Vec<CodexCatalogEntry>>, CodexCatalogError> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(CodexCatalogError::CacheIo),
    };
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_CATALOG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CodexCatalogError::CacheIo)?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(CodexCatalogError::Oversized);
    }
    let document: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| CodexCatalogError::InvalidSchema)?;
    if document.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(CodexCatalogError::InvalidSchema);
    }
    let entries = document
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or(CodexCatalogError::InvalidSchema)?;
    let canonical = serde_json::to_vec(&serde_json::json!({ "models": entries }))
        .map_err(|_| CodexCatalogError::InvalidSchema)?;
    let entries = parse_codex_catalog(&canonical)?;
    if entries.is_empty() {
        return Err(CodexCatalogError::Empty);
    }
    Ok(Some(entries))
}

pub fn should_fetch_live_codex_catalog(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return endpoint.contains("chatgpt.com") || endpoint.contains("chat.openai.com");
    };
    matches!(
        url.host_str(),
        Some("chatgpt.com" | "www.chatgpt.com" | "chat.openai.com")
    )
}

pub async fn fetch_codex_catalog(
    endpoint: &str,
    access_token: &str,
    account_id: &str,
    cache_path: &Path,
) -> Result<CodexCatalogSnapshot, CodexCatalogError> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| CodexCatalogError::Network)?;
    fetch_with_client(endpoint, access_token, account_id, cache_path, &client).await
}

async fn fetch_with_client(
    endpoint: &str,
    access_token: &str,
    account_id: &str,
    cache_path: &Path,
    client: &reqwest::Client,
) -> Result<CodexCatalogSnapshot, CodexCatalogError> {
    let url = codex_models_url(endpoint);
    let mut response = client
        .get(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("chatgpt-account-id", account_id)
        .header("originator", "slim")
        .header("User-Agent", "slim/0.1.0")
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| CodexCatalogError::Network)?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err(CodexCatalogError::Network);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| CodexCatalogError::Network)?
    {
        let next_len = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(CodexCatalogError::Oversized)?;
        if next_len > MAX_CATALOG_BYTES {
            return Err(CodexCatalogError::Oversized);
        }
        bytes.extend_from_slice(&chunk);
    }
    let entries = parse_codex_catalog(&bytes)?;
    if entries.is_empty() {
        return Err(CodexCatalogError::Empty);
    }
    let _ = write_cache(cache_path, &entries);
    if let Ok(mut guard) = MEMORY_CACHE.lock() {
        *guard = Some(MemoryCache {
            key: catalog_key(endpoint, account_id),
            fetched_at: Instant::now(),
            entries: entries.clone(),
        });
    }
    Ok(CodexCatalogSnapshot { entries })
}

fn write_cache(path: &Path, entries: &[CodexCatalogEntry]) -> Result<(), CodexCatalogError> {
    let parent = path.parent().ok_or(CodexCatalogError::UnsafeCache)?;
    fs::create_dir_all(parent).map_err(|_| CodexCatalogError::CacheIo)?;
    let body = serde_json::json!({
        "version": 1,
        "entries": entries.iter().map(|entry| {
            serde_json::json!({
                "slug": entry.slug,
                "context_window": entry.context_window,
            })
        }).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec_pretty(&body).map_err(|_| CodexCatalogError::InvalidSchema)?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(CodexCatalogError::Oversized);
    }
    let temporary = parent.join(format!(
        ".codex-models-{}-{}.tmp",
        std::process::id(),
        entries.len()
    ));
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|_| CodexCatalogError::CacheIo)?;
        file.write_all(&bytes)
            .map_err(|_| CodexCatalogError::CacheIo)?;
        file.sync_all().map_err(|_| CodexCatalogError::CacheIo)?;
    }
    fs::rename(&temporary, path).map_err(|_| CodexCatalogError::CacheIo)
}
