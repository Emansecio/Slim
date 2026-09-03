use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use slim_core::provider::{open_code_model, open_code_models, OPENCODE_GO_MODELS_URL};

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_MODEL_ID_BYTES: usize = 128;
static TEMP_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogError {
    Oversized,
    InvalidSchema,
    InvalidModelId,
    DuplicateModelId,
    Network,
    UnsafeCache,
    CacheIo,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Oversized => "OpenCode Go catalog exceeds its bound",
            Self::InvalidSchema => "OpenCode Go catalog schema is invalid",
            Self::InvalidModelId => "OpenCode Go catalog model id is invalid",
            Self::DuplicateModelId => "OpenCode Go catalog contains a duplicate model id",
            Self::Network => "OpenCode Go catalog request failed",
            Self::UnsafeCache => "OpenCode Go catalog cache path is unsafe",
            Self::CacheIo => "OpenCode Go catalog cache I/O failed",
        })
    }
}

impl std::error::Error for CatalogError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogSource {
    Live,
    Cache,
    Fallback,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSnapshot {
    pub model_ids: Vec<String>,
    pub source: CatalogSource,
}

#[derive(Deserialize)]
struct CatalogDocument {
    object: String,
    data: Vec<CatalogEntry>,
}

#[derive(Deserialize)]
struct CatalogEntry {
    id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CacheDocument {
    version: u32,
    model_ids: Vec<String>,
}

#[derive(Clone)]
pub struct OpenCodeCatalog {
    url: String,
    cache_path: PathBuf,
    client: reqwest::Client,
}

impl OpenCodeCatalog {
    pub fn production() -> Result<Self, CatalogError> {
        let cache_path = if let Some(path) = std::env::var_os("SLIM_OPENCODE_MODELS_FILE") {
            if path.is_empty() {
                return Err(CatalogError::UnsafeCache);
            }
            PathBuf::from(path)
        } else {
            let profile = std::env::var_os("USERPROFILE").ok_or(CatalogError::UnsafeCache)?;
            if profile.is_empty() {
                return Err(CatalogError::UnsafeCache);
            }
            PathBuf::from(profile)
                .join(".slim")
                .join("opencode-go-models.json")
        };
        Self::at(OPENCODE_GO_MODELS_URL, cache_path)
    }

    pub fn at(
        url: impl Into<String>,
        cache_path: impl Into<PathBuf>,
    ) -> Result<Self, CatalogError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| CatalogError::Network)?;
        Ok(Self {
            url: url.into(),
            cache_path: cache_path.into(),
            client,
        })
    }

    pub fn load_or_fallback(&self) -> CatalogSnapshot {
        self.load_cache().unwrap_or_else(|_| CatalogSnapshot {
            model_ids: open_code_models()
                .iter()
                .map(|model| model.id.to_owned())
                .collect(),
            source: CatalogSource::Fallback,
        })
    }

    pub async fn refresh(&self) -> Result<CatalogSnapshot, CatalogError> {
        let mut response = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|_| CatalogError::Network)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
        {
            return Err(CatalogError::Network);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| CatalogError::Network)? {
            let next_len = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or(CatalogError::Oversized)?;
            if next_len > MAX_CATALOG_BYTES {
                return Err(CatalogError::Oversized);
            }
            bytes.extend_from_slice(&chunk);
        }
        let model_ids = parse_catalog(&bytes)?;
        self.write_cache(&model_ids)?;
        Ok(CatalogSnapshot {
            model_ids,
            source: CatalogSource::Live,
        })
    }

    fn load_cache(&self) -> Result<CatalogSnapshot, CatalogError> {
        let metadata = fs::symlink_metadata(&self.cache_path).map_err(|_| CatalogError::CacheIo)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_CATALOG_BYTES as u64
        {
            return Err(CatalogError::UnsafeCache);
        }
        let bytes = fs::read(&self.cache_path).map_err(|_| CatalogError::CacheIo)?;
        let document: CacheDocument =
            serde_json::from_slice(&bytes).map_err(|_| CatalogError::InvalidSchema)?;
        if document.version != 1 {
            return Err(CatalogError::InvalidSchema);
        }
        validate_cached_ids(&document.model_ids)?;
        Ok(CatalogSnapshot {
            model_ids: document.model_ids,
            source: CatalogSource::Cache,
        })
    }

    fn write_cache(&self, model_ids: &[String]) -> Result<(), CatalogError> {
        validate_cached_ids(model_ids)?;
        let parent = self.cache_path.parent().ok_or(CatalogError::UnsafeCache)?;
        fs::create_dir_all(parent).map_err(|_| CatalogError::CacheIo)?;
        if fs::symlink_metadata(&self.cache_path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || metadata.is_dir())
        {
            return Err(CatalogError::UnsafeCache);
        }
        let bytes = serde_json::to_vec_pretty(&CacheDocument {
            version: 1,
            model_ids: model_ids.to_vec(),
        })
        .map_err(|_| CatalogError::InvalidSchema)?;
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(CatalogError::Oversized);
        }
        let temporary = parent.join(format!(
            ".opencode-go-models-{}-{}.tmp",
            std::process::id(),
            TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| CatalogError::CacheIo)?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| CatalogError::CacheIo)?;
            drop(file);
            replace_file(&temporary, &self.cache_path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

pub fn parse_catalog(bytes: &[u8]) -> Result<Vec<String>, CatalogError> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(CatalogError::Oversized);
    }
    let document: CatalogDocument =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::InvalidSchema)?;
    if document.object != "list" || document.data.len() > MAX_CATALOG_ENTRIES {
        return Err(CatalogError::InvalidSchema);
    }
    let mut live = HashSet::with_capacity(document.data.len());
    for entry in document.data {
        validate_model_id(&entry.id)?;
        if !live.insert(entry.id) {
            return Err(CatalogError::DuplicateModelId);
        }
    }
    let supported = open_code_models()
        .iter()
        .filter(|model| live.contains(model.id))
        .map(|model| model.id.to_owned())
        .collect::<Vec<_>>();
    if supported.is_empty() {
        return Err(CatalogError::InvalidSchema);
    }
    Ok(supported)
}

fn validate_cached_ids(model_ids: &[String]) -> Result<(), CatalogError> {
    if model_ids.len() > MAX_CATALOG_ENTRIES {
        return Err(CatalogError::InvalidSchema);
    }
    let mut unique = HashSet::with_capacity(model_ids.len());
    for id in model_ids {
        validate_model_id(id)?;
        if open_code_model(id).is_none() {
            return Err(CatalogError::InvalidModelId);
        }
        if !unique.insert(id) {
            return Err(CatalogError::DuplicateModelId);
        }
    }
    Ok(())
}

fn validate_model_id(id: &str) -> Result<(), CatalogError> {
    if id.is_empty()
        || id.len() > MAX_MODEL_ID_BYTES
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
    {
        Err(CatalogError::InvalidModelId)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), CatalogError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source = source
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let success = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if success == 0 {
        Err(CatalogError::CacheIo)
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), CatalogError> {
    fs::rename(source, destination).map_err(|_| CatalogError::CacheIo)
}
