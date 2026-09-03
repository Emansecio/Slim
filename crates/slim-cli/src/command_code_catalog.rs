use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use slim_core::provider::{
    command_code_models, parse_command_code_catalog, CommandCodeCatalogEntry,
    COMMANDCODE_MODELS_URL,
};

const MAX_CATALOG_BYTES: usize = 1024 * 1024;
static TEMP_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogSource {
    Live,
    Cache,
    Fallback,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSnapshot {
    pub models: Vec<CommandCodeCatalogEntry>,
    pub source: CatalogSource,
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
}

#[derive(Clone)]
pub struct CommandCodeCatalog {
    url: String,
    cache_path: PathBuf,
    client: reqwest::Client,
}

impl CommandCodeCatalog {
    pub fn production() -> Result<Self, slim_core::ProviderError> {
        let cache_path = if let Some(path) = std::env::var_os("SLIM_COMMANDCODE_MODELS_FILE") {
            PathBuf::from(path)
        } else {
            let profile = std::env::var_os("USERPROFILE").ok_or_else(|| {
                slim_core::ProviderError::InvalidResponse {
                    message: "Command Code catalog cache path is unsafe".into(),
                }
            })?;
            PathBuf::from(profile)
                .join(".slim")
                .join("command-code-models.json")
        };
        Self::at(COMMANDCODE_MODELS_URL, cache_path)
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
            .map_err(|_| slim_core::ProviderError::InvalidResponse {
                message: "Command Code catalog request failed".into(),
            })?;
        Ok(Self {
            url: url.into(),
            cache_path: cache_path.into(),
            client,
        })
    }

    pub fn load_or_fallback(&self) -> CatalogSnapshot {
        self.load_cache().unwrap_or_else(|_| CatalogSnapshot {
            models: command_code_models()
                .iter()
                .map(|model| CommandCodeCatalogEntry {
                    id: model.id.to_owned(),
                    name: model.name.to_owned(),
                    context_window: model.context_window,
                })
                .collect(),
            source: CatalogSource::Fallback,
        })
    }

    pub async fn refresh(&self) -> Result<CatalogSnapshot, slim_core::ProviderError> {
        let mut response = self.client.get(&self.url).send().await.map_err(|_| {
            slim_core::ProviderError::InvalidResponse {
                message: "Command Code catalog request failed".into(),
            }
        })?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
        {
            return Err(slim_core::ProviderError::InvalidResponse {
                message: "Command Code catalog request failed".into(),
            });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) =
            response
                .chunk()
                .await
                .map_err(|_| slim_core::ProviderError::InvalidResponse {
                    message: "Command Code catalog request failed".into(),
                })?
        {
            let next_len = bytes.len().saturating_add(chunk.len());
            if next_len > MAX_CATALOG_BYTES {
                return Err(slim_core::ProviderError::InvalidResponse {
                    message: "Command Code catalog exceeds its bound".into(),
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        let models = parse_command_code_catalog(&bytes)?;
        self.write_cache(&models)?;
        Ok(CatalogSnapshot {
            models,
            source: CatalogSource::Live,
        })
    }

    fn load_cache(&self) -> Result<CatalogSnapshot, slim_core::ProviderError> {
        let metadata = fs::symlink_metadata(&self.cache_path).map_err(|_| {
            slim_core::ProviderError::InvalidResponse {
                message: "Command Code catalog cache I/O failed".into(),
            }
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_CATALOG_BYTES as u64
        {
            return Err(slim_core::ProviderError::InvalidResponse {
                message: "Command Code catalog cache path is unsafe".into(),
            });
        }
        let bytes =
            fs::read(&self.cache_path).map_err(|_| slim_core::ProviderError::InvalidResponse {
                message: "Command Code catalog cache I/O failed".into(),
            })?;
        let document: CacheDocument = serde_json::from_slice(&bytes).map_err(|_| {
            slim_core::ProviderError::InvalidResponse {
                message: "Command Code catalog schema is invalid".into(),
            }
        })?;
        if document.version != 1 {
            return Err(slim_core::ProviderError::InvalidResponse {
                message: "Command Code catalog schema is invalid".into(),
            });
        }
        Ok(CatalogSnapshot {
            models: document
                .models
                .into_iter()
                .map(|entry| CommandCodeCatalogEntry {
                    id: entry.id,
                    name: entry.name,
                    context_window: entry.context_window.max(1),
                })
                .collect(),
            source: CatalogSource::Cache,
        })
    }

    fn write_cache(
        &self,
        models: &[CommandCodeCatalogEntry],
    ) -> Result<(), slim_core::ProviderError> {
        let parent =
            self.cache_path
                .parent()
                .ok_or_else(|| slim_core::ProviderError::InvalidResponse {
                    message: "Command Code catalog cache path is unsafe".into(),
                })?;
        fs::create_dir_all(parent).map_err(|_| slim_core::ProviderError::InvalidResponse {
            message: "Command Code catalog cache I/O failed".into(),
        })?;
        let bytes = serde_json::to_vec_pretty(&CacheDocument {
            version: 1,
            models: models
                .iter()
                .map(|model| CacheEntry {
                    id: model.id.clone(),
                    name: model.name.clone(),
                    context_window: model.context_window,
                })
                .collect(),
        })
        .map_err(|_| slim_core::ProviderError::InvalidResponse {
            message: "Command Code catalog schema is invalid".into(),
        })?;
        let temporary = parent.join(format!(
            ".command-code-models-{}-{}.tmp",
            std::process::id(),
            TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| slim_core::ProviderError::InvalidResponse {
                    message: "Command Code catalog cache I/O failed".into(),
                })?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| slim_core::ProviderError::InvalidResponse {
                    message: "Command Code catalog cache I/O failed".into(),
                })?;
            drop(file);
            fs::rename(&temporary, &self.cache_path).map_err(|_| {
                slim_core::ProviderError::InvalidResponse {
                    message: "Command Code catalog cache I/O failed".into(),
                }
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}
