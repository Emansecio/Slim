use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use slim_core::provider::{
    devin_models, fetch_devin_model_configs, DevinCatalogEntry, DEVIN_DEFAULT_BASE_URL,
};

pub use crate::command_code_catalog::CatalogSource;

const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;
static TEMP_NONCE: AtomicU64 = AtomicU64::new(1);
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DevinCatalogSnapshot {
    pub models: Vec<DevinCatalogEntry>,
    pub source: CatalogSource,
}

impl DevinCatalogSnapshot {
    pub fn find(&self, id: &str) -> Option<&DevinCatalogEntry> {
        self.models.iter().find(|model| model.id == id)
    }
}

#[derive(Deserialize, Serialize)]
struct CacheDocument {
    version: u32,
    models: Vec<CacheEntry>,
}

#[derive(Deserialize, Serialize)]
struct CacheEntry {
    id: String,
    name: String,
    context_window: u64,
    max_output_tokens: u32,
    #[serde(default)]
    accepts_images: bool,
    #[serde(default)]
    model_router: bool,
    #[serde(default)]
    default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    effort_routes: Vec<(String, String)>,
    #[serde(default)]
    parallel_tool_calls: bool,
}

#[derive(Clone)]
pub struct DevinCatalog {
    url: String,
    cache_path: PathBuf,
    client: reqwest::Client,
}

fn failure(message: &str) -> slim_core::ProviderError {
    slim_core::ProviderError::InvalidResponse {
        message: format!("Devin catalog {message}"),
    }
}

impl DevinCatalog {
    pub fn production() -> Result<Self, slim_core::ProviderError> {
        let cache_path = if let Some(path) = std::env::var_os("SLIM_DEVIN_MODELS_FILE") {
            PathBuf::from(path)
        } else {
            let profile =
                crate::auth::user_home_var().ok_or_else(|| failure("cache path is unsafe"))?;
            PathBuf::from(profile)
                .join(".slim")
                .join("devin-models.json")
        };
        Self::at(DEVIN_DEFAULT_BASE_URL, cache_path)
    }

    pub fn at(
        url: impl Into<String>,
        cache_path: impl Into<PathBuf>,
    ) -> Result<Self, slim_core::ProviderError> {
        let client = slim_core::provider::shared_http_client(Duration::from_secs(15))
            .map_err(|_| failure("request failed"))?;
        Ok(Self {
            url: url.into(),
            cache_path: cache_path.into(),
            client,
        })
    }

    pub fn load_or_fallback(&self) -> DevinCatalogSnapshot {
        self.load_cache()
            .unwrap_or_else(|_| DevinCatalogSnapshot {
                models: devin_models()
                    .iter()
                    .map(|model| DevinCatalogEntry {
                        id: model.id.to_owned(),
                        name: model.name.to_owned(),
                        context_window: model.context_window,
                        max_output_tokens: model.max_output_tokens,
                        accepts_images: model.accepts_images,
                        model_router: model.model_router,
                        default: model.id == slim_core::provider::DEVIN_DEFAULT_MODEL,
                        ..DevinCatalogEntry::default()
                    })
                    .collect(),
                source: CatalogSource::Fallback,
            })
    }

    pub fn cache_is_fresh(&self, ttl: Duration) -> bool {
        fs::symlink_metadata(&self.cache_path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age <= ttl)
    }

    pub async fn refresh(
        &self,
        api_key: &str,
    ) -> Result<DevinCatalogSnapshot, slim_core::ProviderError> {
        let _guard = REFRESH_LOCK.lock().await;
        self.refresh_unlocked(api_key).await
    }

    pub async fn refresh_if_stale(
        &self,
        api_key: &str,
        ttl: Duration,
    ) -> Result<Option<DevinCatalogSnapshot>, slim_core::ProviderError> {
        if self.cache_is_fresh(ttl) {
            return Ok(None);
        }
        let _guard = REFRESH_LOCK.lock().await;
        if self.cache_is_fresh(ttl) {
            return Ok(None);
        }
        self.refresh_unlocked(api_key).await.map(Some)
    }

    async fn refresh_unlocked(
        &self,
        api_key: &str,
    ) -> Result<DevinCatalogSnapshot, slim_core::ProviderError> {
        let models = fetch_devin_model_configs(&self.client, &self.url, api_key).await?;
        self.write_cache(&models)?;
        Ok(DevinCatalogSnapshot {
            models,
            source: CatalogSource::Live,
        })
    }

    fn load_cache(&self) -> Result<DevinCatalogSnapshot, slim_core::ProviderError> {
        let metadata =
            fs::symlink_metadata(&self.cache_path).map_err(|_| failure("cache I/O failed"))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_CATALOG_BYTES as u64
        {
            return Err(failure("cache path is unsafe"));
        }
        let bytes = fs::read(&self.cache_path).map_err(|_| failure("cache I/O failed"))?;
        let document: CacheDocument =
            serde_json::from_slice(&bytes).map_err(|_| failure("schema is invalid"))?;
        if document.version != 1 || document.models.is_empty() {
            return Err(failure("schema is invalid"));
        }
        Ok(DevinCatalogSnapshot {
            models: document
                .models
                .into_iter()
                .filter(|entry| slim_core::provider::is_devin_model_id(&entry.id))
                .map(|entry| DevinCatalogEntry {
                    id: entry.id,
                    name: entry.name,
                    context_window: entry.context_window.max(1),
                    max_output_tokens: entry.max_output_tokens.max(1),
                    accepts_images: entry.accepts_images,
                    model_router: entry.model_router,
                    default: entry.default,
                    request_model: entry.request_model,
                    effort_routes: entry.effort_routes,
                    parallel_tool_calls: entry.parallel_tool_calls,
                })
                .collect(),
            source: CatalogSource::Cache,
        })
    }

    fn write_cache(&self, models: &[DevinCatalogEntry]) -> Result<(), slim_core::ProviderError> {
        let parent = self
            .cache_path
            .parent()
            .ok_or_else(|| failure("cache path is unsafe"))?;
        fs::create_dir_all(parent).map_err(|_| failure("cache I/O failed"))?;
        let bytes = serde_json::to_vec_pretty(&CacheDocument {
            version: 1,
            models: models
                .iter()
                .map(|model| CacheEntry {
                    id: model.id.clone(),
                    name: model.name.clone(),
                    context_window: model.context_window,
                    max_output_tokens: model.max_output_tokens,
                    accepts_images: model.accepts_images,
                    model_router: model.model_router,
                    default: model.default,
                    request_model: model.request_model.clone(),
                    effort_routes: model.effort_routes.clone(),
                    parallel_tool_calls: model.parallel_tool_calls,
                })
                .collect(),
        })
        .map_err(|_| failure("schema is invalid"))?;
        let temporary = parent.join(format!(
            ".devin-models-{}-{}.tmp",
            std::process::id(),
            TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| failure("cache I/O failed"))?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| failure("cache I/O failed"))?;
            drop(file);
            fs::rename(&temporary, &self.cache_path).map_err(|_| failure("cache I/O failed"))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trips_and_falls_back_without_a_file() {
        let dir =
            std::env::temp_dir().join(format!("slim-devin-catalog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("devin-models.json");
        let catalog = DevinCatalog::at("http://127.0.0.1:9", &path).unwrap();
        assert!(!catalog.cache_is_fresh(Duration::from_secs(60)));
        let fallback = catalog.load_or_fallback();
        assert_eq!(fallback.source, CatalogSource::Fallback);
        assert!(fallback
            .find(slim_core::provider::DEVIN_DEFAULT_MODEL)
            .is_some());

        let models = vec![DevinCatalogEntry {
            id: "vendor/new-model".into(),
            name: "New Model".into(),
            context_window: 400_000,
            max_output_tokens: 32_000,
            accepts_images: true,
            model_router: true,
            default: false,
            request_model: Some("vendor/new-model-high".into()),
            effort_routes: vec![("high".into(), "vendor/new-model-high".into())],
            parallel_tool_calls: true,
        }];
        catalog.write_cache(&models).unwrap();
        assert!(catalog.cache_is_fresh(Duration::from_secs(60)));
        assert!(!catalog.cache_is_fresh(Duration::ZERO));
        let cached = catalog.load_or_fallback();
        assert_eq!(cached.source, CatalogSource::Cache);
        assert_eq!(cached.models, models);
        let _ = fs::remove_dir_all(&dir);
    }
}
