//! Shared implementation behind the OpenCode Go and Zen model catalogs. Each
//! public catalog module only supplies a [`CatalogSpec`] (label, cache file,
//! URL and local model metadata); parsing, cache I/O and refresh live here.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use slim_core::provider::OpenCodeModel;

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_MODEL_ID_BYTES: usize = 128;
static TEMP_NONCE: AtomicU64 = AtomicU64::new(1);

/// What differs between the catalogs: naming and the local model metadata.
pub(crate) struct CatalogSpec {
    /// Provider name used in error messages, e.g. `OpenCode Go`.
    pub label: &'static str,
    /// Environment variable that overrides the cache file path.
    pub cache_env: &'static str,
    /// Cache file name under `~/.slim`.
    pub cache_file: &'static str,
    /// Prefix of the temporary file written next to the cache.
    pub temp_prefix: &'static str,
    pub url: &'static str,
    /// Models with verified local metadata; the live list is filtered by it.
    pub models: fn() -> &'static [OpenCodeModel],
    pub model: fn(&str) -> Option<&'static OpenCodeModel>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogErrorKind {
    Oversized,
    InvalidSchema,
    InvalidModelId,
    DuplicateModelId,
    Network,
    UnsafeCache,
    CacheIo,
}

/// A catalog failure; the message names the provider it came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogError {
    kind: CatalogErrorKind,
    label: &'static str,
}

impl CatalogError {
    pub fn kind(&self) -> CatalogErrorKind {
        self.kind
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = self.label;
        match self.kind {
            CatalogErrorKind::Oversized => write!(formatter, "{label} catalog exceeds its bound"),
            CatalogErrorKind::InvalidSchema => {
                write!(formatter, "{label} catalog schema is invalid")
            }
            CatalogErrorKind::InvalidModelId => {
                write!(formatter, "{label} catalog model id is invalid")
            }
            CatalogErrorKind::DuplicateModelId => {
                write!(formatter, "{label} catalog contains a duplicate model id")
            }
            CatalogErrorKind::Network => write!(formatter, "{label} catalog request failed"),
            CatalogErrorKind::UnsafeCache => {
                write!(formatter, "{label} catalog cache path is unsafe")
            }
            CatalogErrorKind::CacheIo => write!(formatter, "{label} catalog cache I/O failed"),
        }
    }
}

impl std::error::Error for CatalogError {}

type Kind = CatalogErrorKind;

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
pub(crate) struct CatalogClient {
    spec: &'static CatalogSpec,
    url: String,
    cache_path: PathBuf,
    client: reqwest::Client,
}

impl CatalogClient {
    pub(crate) fn production(spec: &'static CatalogSpec) -> Result<Self, CatalogError> {
        let cache_path = if let Some(path) = std::env::var_os(spec.cache_env) {
            if path.is_empty() {
                return Err(spec.error(Kind::UnsafeCache));
            }
            PathBuf::from(path)
        } else {
            let profile =
                crate::auth::user_home_var().ok_or_else(|| spec.error(Kind::UnsafeCache))?;
            if profile.is_empty() {
                return Err(spec.error(Kind::UnsafeCache));
            }
            PathBuf::from(profile).join(".slim").join(spec.cache_file)
        };
        Self::at(spec, spec.url, cache_path)
    }

    pub(crate) fn at(
        spec: &'static CatalogSpec,
        url: impl Into<String>,
        cache_path: impl Into<PathBuf>,
    ) -> Result<Self, CatalogError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| spec.error(Kind::Network))?;
        Ok(Self {
            spec,
            url: url.into(),
            cache_path: cache_path.into(),
            client,
        })
    }

    pub(crate) fn load_or_fallback(&self) -> CatalogSnapshot {
        self.load_cache().unwrap_or_else(|_| CatalogSnapshot {
            model_ids: (self.spec.models)()
                .iter()
                .map(|model| model.id.to_owned())
                .collect(),
            source: CatalogSource::Fallback,
        })
    }

    pub(crate) async fn refresh(&self) -> Result<CatalogSnapshot, CatalogError> {
        self.refresh_kind()
            .await
            .map_err(|kind| self.spec.error(kind))
    }

    async fn refresh_kind(&self) -> Result<CatalogSnapshot, Kind> {
        let mut response = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|_| Kind::Network)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
        {
            return Err(Kind::Network);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Kind::Network)? {
            let next_len = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or(Kind::Oversized)?;
            if next_len > MAX_CATALOG_BYTES {
                return Err(Kind::Oversized);
            }
            bytes.extend_from_slice(&chunk);
        }
        let model_ids = parse_kind(self.spec, &bytes)?;
        self.write_cache(&model_ids)?;
        Ok(CatalogSnapshot {
            model_ids,
            source: CatalogSource::Live,
        })
    }

    fn load_cache(&self) -> Result<CatalogSnapshot, Kind> {
        let metadata = fs::symlink_metadata(&self.cache_path).map_err(|_| Kind::CacheIo)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_CATALOG_BYTES as u64
        {
            return Err(Kind::UnsafeCache);
        }
        let bytes = fs::read(&self.cache_path).map_err(|_| Kind::CacheIo)?;
        let document: CacheDocument =
            serde_json::from_slice(&bytes).map_err(|_| Kind::InvalidSchema)?;
        if document.version != 1 {
            return Err(Kind::InvalidSchema);
        }
        validate_cached_ids(self.spec, &document.model_ids)?;
        Ok(CatalogSnapshot {
            model_ids: document.model_ids,
            source: CatalogSource::Cache,
        })
    }

    fn write_cache(&self, model_ids: &[String]) -> Result<(), Kind> {
        validate_cached_ids(self.spec, model_ids)?;
        let parent = self.cache_path.parent().ok_or(Kind::UnsafeCache)?;
        fs::create_dir_all(parent).map_err(|_| Kind::CacheIo)?;
        if fs::symlink_metadata(&self.cache_path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || metadata.is_dir())
        {
            return Err(Kind::UnsafeCache);
        }
        let bytes = serde_json::to_vec_pretty(&CacheDocument {
            version: 1,
            model_ids: model_ids.to_vec(),
        })
        .map_err(|_| Kind::InvalidSchema)?;
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(Kind::Oversized);
        }
        let temporary = parent.join(format!(
            "{}{}-{}.tmp",
            self.spec.temp_prefix,
            std::process::id(),
            TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| Kind::CacheIo)?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| Kind::CacheIo)?;
            drop(file);
            replace_file(&temporary, &self.cache_path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

impl CatalogSpec {
    fn error(&'static self, kind: Kind) -> CatalogError {
        CatalogError {
            kind,
            label: self.label,
        }
    }
}

pub(crate) fn parse_catalog(
    spec: &'static CatalogSpec,
    bytes: &[u8],
) -> Result<Vec<String>, CatalogError> {
    parse_kind(spec, bytes).map_err(|kind| spec.error(kind))
}

fn parse_kind(spec: &CatalogSpec, bytes: &[u8]) -> Result<Vec<String>, Kind> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(Kind::Oversized);
    }
    let document: CatalogDocument =
        serde_json::from_slice(bytes).map_err(|_| Kind::InvalidSchema)?;
    if document.object != "list" || document.data.len() > MAX_CATALOG_ENTRIES {
        return Err(Kind::InvalidSchema);
    }
    let mut live = HashSet::with_capacity(document.data.len());
    for entry in document.data {
        validate_model_id(&entry.id)?;
        if !live.insert(entry.id) {
            return Err(Kind::DuplicateModelId);
        }
    }
    let supported = (spec.models)()
        .iter()
        .filter(|model| live.contains(model.id))
        .map(|model| model.id.to_owned())
        .collect::<Vec<_>>();
    if supported.is_empty() {
        return Err(Kind::InvalidSchema);
    }
    Ok(supported)
}

fn validate_cached_ids(spec: &CatalogSpec, model_ids: &[String]) -> Result<(), Kind> {
    if model_ids.len() > MAX_CATALOG_ENTRIES {
        return Err(Kind::InvalidSchema);
    }
    let mut unique = HashSet::with_capacity(model_ids.len());
    for id in model_ids {
        validate_model_id(id)?;
        if (spec.model)(id).is_none() {
            return Err(Kind::InvalidModelId);
        }
        if !unique.insert(id) {
            return Err(Kind::DuplicateModelId);
        }
    }
    Ok(())
}

fn validate_model_id(id: &str) -> Result<(), Kind> {
    if id.is_empty()
        || id.len() > MAX_MODEL_ID_BYTES
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
    {
        Err(Kind::InvalidModelId)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), Kind> {
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
        Err(Kind::CacheIo)
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), Kind> {
    fs::rename(source, destination).map_err(|_| Kind::CacheIo)
}
