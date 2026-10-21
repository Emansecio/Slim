use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use slim_core::provider::{
    fetch_open_design_catalog, open_design_models, OpenDesignCatalogEntry, OPEN_DESIGN_MODELS_URL,
};

pub use crate::command_code_catalog::CatalogSource;

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
static TEMP_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenDesignCatalogSnapshot {
    pub models: Vec<OpenDesignCatalogEntry>,
    pub source: CatalogSource,
}

impl OpenDesignCatalogSnapshot {
    pub fn find(&self, id: &str) -> Option<&OpenDesignCatalogEntry> {
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
    default: bool,
}

#[derive(Clone)]
pub struct OpenDesignCatalog {
    url: String,
    cache_path: PathBuf,
    client: reqwest::Client,
}

fn failure(message: &str) -> slim_core::ProviderError {
    slim_core::ProviderError::InvalidResponse {
        message: format!("Open Design catalog {message}"),
    }
}

impl OpenDesignCatalog {
    pub fn production() -> Result<Self, slim_core::ProviderError> {
        let cache_path = if let Some(path) = std::env::var_os("SLIM_OPEN_DESIGN_MODELS_FILE") {
            PathBuf::from(path)
        } else {
            let profile =
                crate::auth::user_home_var().ok_or_else(|| failure("cache path is unsafe"))?;
            PathBuf::from(profile)
                .join(".slim")
                .join("open-design-models.json")
        };
        Self::at(OPEN_DESIGN_MODELS_URL, cache_path)
    }

    pub fn at(
        url: impl Into<String>,
        cache_path: impl Into<PathBuf>,
    ) -> Result<Self, slim_core::ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| failure("request failed"))?;
        Ok(Self {
            url: url.into(),
            cache_path: cache_path.into(),
            client,
        })
    }

    pub fn load_or_fallback(&self) -> OpenDesignCatalogSnapshot {
        self.load_cache()
            .unwrap_or_else(|_| OpenDesignCatalogSnapshot {
                models: open_design_models()
                    .iter()
                    .map(|model| OpenDesignCatalogEntry {
                        id: model.id.to_owned(),
                        name: model.name.to_owned(),
                        context_window: model.context_window,
                        max_output_tokens: model.max_output_tokens,
                        accepts_images: model.accepts_images,
                        default: model.id == slim_core::provider::OPEN_DESIGN_DEFAULT_MODEL,
                    })
                    .collect(),
                source: CatalogSource::Fallback,
            })
    }

    pub async fn refresh(
        &self,
        api_key: &str,
    ) -> Result<OpenDesignCatalogSnapshot, slim_core::ProviderError> {
        let models = fetch_open_design_catalog(&self.client, &self.url, api_key).await?;
        self.write_cache(&models)?;
        Ok(OpenDesignCatalogSnapshot {
            models,
            source: CatalogSource::Live,
        })
    }

    fn load_cache(&self) -> Result<OpenDesignCatalogSnapshot, slim_core::ProviderError> {
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
        Ok(OpenDesignCatalogSnapshot {
            models: document
                .models
                .into_iter()
                .filter(|entry| slim_core::provider::is_open_design_model_id(&entry.id))
                .map(|entry| OpenDesignCatalogEntry {
                    id: entry.id,
                    name: entry.name,
                    context_window: entry.context_window.max(1),
                    max_output_tokens: entry.max_output_tokens.max(1),
                    accepts_images: entry.accepts_images,
                    default: entry.default,
                })
                .collect(),
            source: CatalogSource::Cache,
        })
    }

    fn write_cache(
        &self,
        models: &[OpenDesignCatalogEntry],
    ) -> Result<(), slim_core::ProviderError> {
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
                    default: model.default,
                })
                .collect(),
        })
        .map_err(|_| failure("schema is invalid"))?;
        let temporary = parent.join(format!(
            ".open-design-models-{}-{}.tmp",
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
            std::env::temp_dir().join(format!("slim-open-design-catalog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("open-design-models.json");
        let catalog = OpenDesignCatalog::at("http://127.0.0.1:9/v1/models", &path).unwrap();
        let fallback = catalog.load_or_fallback();
        assert_eq!(fallback.source, CatalogSource::Fallback);
        assert!(fallback
            .find(slim_core::provider::OPEN_DESIGN_DEFAULT_MODEL)
            .is_some());

        let models = vec![OpenDesignCatalogEntry {
            id: "vendor/new-model".into(),
            name: "New Model".into(),
            context_window: 400_000,
            max_output_tokens: 32_000,
            accepts_images: true,
            default: false,
        }];
        catalog.write_cache(&models).unwrap();
        let cached = catalog.load_or_fallback();
        assert_eq!(cached.source, CatalogSource::Cache);
        assert_eq!(cached.models, models);
        let _ = fs::remove_dir_all(&dir);
    }
}
