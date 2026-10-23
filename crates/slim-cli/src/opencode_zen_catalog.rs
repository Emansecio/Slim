use std::path::PathBuf;

use slim_core::provider::{zen_model, zen_models, OPENCODE_ZEN_MODELS_URL};

use crate::opencode_catalog::{CatalogClient, CatalogSpec};
pub use crate::opencode_catalog::{CatalogError, CatalogErrorKind, CatalogSnapshot, CatalogSource};

static SPEC: CatalogSpec = CatalogSpec {
    label: "OpenCode Zen",
    cache_env: "SLIM_OPENCODE_ZEN_MODELS_FILE",
    cache_file: "opencode-zen-models.json",
    temp_prefix: ".opencode-zen-models-",
    url: OPENCODE_ZEN_MODELS_URL,
    models: zen_models,
    model: zen_model,
};

#[derive(Clone)]
pub struct OpenCodeZenCatalog(CatalogClient);

impl OpenCodeZenCatalog {
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

/// The live catalog lists every Zen model (paid included); the selector keeps
/// only ids with verified local metadata — the zero-cost set.
pub fn parse_catalog(bytes: &[u8]) -> Result<Vec<String>, CatalogError> {
    crate::opencode_catalog::parse_catalog(&SPEC, bytes)
}
