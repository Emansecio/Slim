use std::path::PathBuf;

use slim_core::provider::{open_code_model, open_code_models, OPENCODE_GO_MODELS_URL};

use crate::opencode_catalog::{CatalogClient, CatalogSpec};
pub use crate::opencode_catalog::{CatalogError, CatalogErrorKind, CatalogSnapshot, CatalogSource};

static SPEC: CatalogSpec = CatalogSpec {
    label: "OpenCode Go",
    cache_env: "SLIM_OPENCODE_MODELS_FILE",
    cache_file: "opencode-go-models.json",
    temp_prefix: ".opencode-go-models-",
    url: OPENCODE_GO_MODELS_URL,
    models: open_code_models,
    model: open_code_model,
};

#[derive(Clone)]
pub struct OpenCodeCatalog(CatalogClient);

impl OpenCodeCatalog {
    pub fn production() -> Result<Self, CatalogError> {
        CatalogClient::production(&SPEC).map(Self)
    }

    pub fn at(
        url: impl Into<String>,
        cache_path: impl Into<PathBuf>,
    ) -> Result<Self, CatalogError> {
        CatalogClient::at(&SPEC, url, cache_path).map(Self)
    }

    pub fn load_or_fallback(&self) -> CatalogSnapshot {
        self.0.load_or_fallback()
    }

    pub async fn refresh(&self) -> Result<CatalogSnapshot, CatalogError> {
        self.0.refresh().await
    }
}

pub fn parse_catalog(bytes: &[u8]) -> Result<Vec<String>, CatalogError> {
    crate::opencode_catalog::parse_catalog(&SPEC, bytes)
}
