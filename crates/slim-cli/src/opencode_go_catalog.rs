use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use slim_core::provider::{
    is_trusted_opencode_base_url, open_code_model, open_code_models, register_open_code_models,
    resolve_open_code_model, OpenCodeApi, OpenCodeModelSpec, OPENCODE_GO_CONSOLE_MODELS_URL,
    OPENCODE_GO_MODELS_URL, OPENCODE_GO_OPENAI_BASE_URL,
};

pub use crate::opencode_catalog::{CatalogErrorKind, CatalogSource};

const LABEL: &str = "OpenCode Go";
const CACHE_ENV: &str = "SLIM_OPENCODE_MODELS_FILE";
const CACHE_FILE: &str = "opencode-go-models.json";
const CACHE_VERSION: u32 = 2;
const MAX_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_CATALOG_ENTRIES: usize = 256;
const MAX_MODEL_ID_BYTES: usize = 128;
const SUPPORTED_REASONING_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
static TEMP_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogError {
    kind: CatalogErrorKind,
}

impl CatalogError {
    fn new(kind: CatalogErrorKind) -> Self {
        Self { kind }
    }

    pub fn kind(&self) -> CatalogErrorKind {
        self.kind
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            CatalogErrorKind::Oversized => write!(formatter, "{LABEL} catalog exceeds its bound"),
            CatalogErrorKind::InvalidSchema => {
                write!(formatter, "{LABEL} catalog schema is invalid")
            }
            CatalogErrorKind::InvalidModelId => {
                write!(formatter, "{LABEL} catalog model id is invalid")
            }
            CatalogErrorKind::DuplicateModelId => {
                write!(formatter, "{LABEL} catalog contains a duplicate model id")
            }
            CatalogErrorKind::Network => write!(formatter, "{LABEL} catalog request failed"),
            CatalogErrorKind::UnsafeCache => {
                write!(formatter, "{LABEL} catalog cache path is unsafe")
            }
            CatalogErrorKind::CacheIo => write!(formatter, "{LABEL} catalog cache I/O failed"),
        }
    }
}

impl std::error::Error for CatalogError {}

type Kind = CatalogErrorKind;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSnapshot {
    pub models: Vec<OpenCodeModelSpec>,
    pub source: CatalogSource,
}

impl CatalogSnapshot {
    pub fn model_ids(&self) -> Vec<String> {
        self.models.iter().map(|model| model.id.clone()).collect()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CacheDocument {
    version: u32,
    models: Vec<OpenCodeModelSpec>,
}

enum FetchError {
    Unauthorized,
    Other(Kind),
}

#[derive(Clone)]
pub struct OpenCodeCatalog {
    console_url: Option<String>,
    url: String,
    cache_path: PathBuf,
    client: reqwest::Client,
}

pub(crate) fn cache_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CACHE_ENV) {
        return (!path.is_empty()).then(|| PathBuf::from(path));
    }
    let profile = crate::auth::user_home_var()?;
    (!profile.is_empty()).then(|| PathBuf::from(profile).join(".slim").join(CACHE_FILE))
}

/// Resolves an OpenCode Go model id against the live registered catalog, the
/// on-disk cache (`~/.slim/opencode-go-models.json`), and finally the bundled
/// fallback registry.
pub fn resolve_model(id: &str) -> Option<OpenCodeModelSpec> {
    if let Some(spec) = resolve_open_code_model(id) {
        return Some(spec);
    }
    if let Some(path) = cache_path() {
        if let Ok(models) = read_cache_file(&path) {
            register_open_code_models(&models);
            if let Some(spec) = resolve_open_code_model(id) {
                return Some(spec);
            }
        }
    }
    None
}

pub fn known_model(id: &str) -> bool {
    resolve_model(id).is_some()
}

impl OpenCodeCatalog {
    pub fn production() -> Result<Self, CatalogError> {
        let cache_path = cache_path().ok_or_else(|| CatalogError::new(Kind::UnsafeCache))?;
        Self::at_with_console(OPENCODE_GO_CONSOLE_MODELS_URL, OPENCODE_GO_MODELS_URL, cache_path)
    }

    pub fn at(
        url: impl Into<String>,
        cache_path: impl Into<PathBuf>,
    ) -> Result<Self, CatalogError> {
        let client = slim_core::provider::shared_http_client(Duration::from_secs(15))
            .map_err(|_| CatalogError::new(Kind::Network))?;
        Ok(Self {
            console_url: None,
            url: url.into(),
            cache_path: cache_path.into(),
            client,
        })
    }

    pub fn at_with_console(
        console_url: impl Into<String>,
        url: impl Into<String>,
        cache_path: impl Into<PathBuf>,
    ) -> Result<Self, CatalogError> {
        let client = slim_core::provider::shared_http_client(Duration::from_secs(15))
            .map_err(|_| CatalogError::new(Kind::Network))?;
        Ok(Self {
            console_url: Some(console_url.into()),
            url: url.into(),
            cache_path: cache_path.into(),
            client,
        })
    }

    pub fn load_or_fallback(&self) -> CatalogSnapshot {
        match read_cache_file(&self.cache_path) {
            Ok(models) => {
                register_open_code_models(&models);
                CatalogSnapshot {
                    models,
                    source: CatalogSource::Cache,
                }
            }
            Err(_) => CatalogSnapshot {
                models: open_code_models()
                    .iter()
                    .map(OpenCodeModelSpec::from)
                    .collect(),
                source: CatalogSource::Fallback,
            },
        }
    }

    pub async fn refresh(&self) -> Result<CatalogSnapshot, CatalogError> {
        self.refresh_with_key(None).await
    }

    pub async fn refresh_with_key(
        &self,
        api_key: Option<&str>,
    ) -> Result<CatalogSnapshot, CatalogError> {
        let key = api_key.map(str::trim).filter(|k| !k.is_empty());
        // When authenticated, prefer the Console model list API
        // (`/console/api/v1/models`) so workspace-specific Go availability and
        // limits match OpenCode; fall back to `/inference/go/v1/models` if the
        // Console route is unavailable or restricted.
        if let (Some(console_url), Some(key)) = (self.console_url.as_deref(), key) {
            if let Ok(models) = self.fetch_and_parse(console_url, Some(key)).await {
                self.publish(&models).map_err(CatalogError::new)?;
                return Ok(CatalogSnapshot {
                    models,
                    source: CatalogSource::Live,
                });
            }
        }
        let models = match self.fetch_and_parse(&self.url, key).await {
            Ok(models) => models,
            Err(FetchError::Unauthorized) if key.is_some() => self
                .fetch_and_parse(&self.url, None)
                .await
                .map_err(|error| CatalogError::new(error.into_kind()))?,
            Err(error) => return Err(CatalogError::new(error.into_kind())),
        };
        self.publish(&models).map_err(CatalogError::new)?;
        Ok(CatalogSnapshot {
            models,
            source: CatalogSource::Live,
        })
    }

    fn publish(&self, models: &[OpenCodeModelSpec]) -> Result<(), Kind> {
        self.write_cache(models)?;
        register_open_code_models(models);
        Ok(())
    }

    async fn fetch_and_parse(
        &self,
        url: &str,
        api_key: Option<&str>,
    ) -> Result<Vec<OpenCodeModelSpec>, FetchError> {
        let mut request = self.client.get(url).timeout(Duration::from_secs(15));
        if let Some(key) = api_key {
            request = request.bearer_auth(key);
        }
        let mut response = request.send().await.map_err(|_| FetchError::Other(Kind::Network))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(FetchError::Unauthorized);
        }
        if !status.is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
        {
            return Err(FetchError::Other(Kind::Network));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| FetchError::Other(Kind::Network))?
        {
            let next_len = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or(FetchError::Other(Kind::Oversized))?;
            if next_len > MAX_CATALOG_BYTES {
                return Err(FetchError::Other(Kind::Oversized));
            }
            bytes.extend_from_slice(&chunk);
        }
        parse_specs_kind(&bytes).map_err(FetchError::Other)
    }

    fn write_cache(&self, models: &[OpenCodeModelSpec]) -> Result<(), Kind> {
        validate_cached_specs(models)?;
        let parent = self.cache_path.parent().ok_or(Kind::UnsafeCache)?;
        fs::create_dir_all(parent).map_err(|_| Kind::CacheIo)?;
        if fs::symlink_metadata(&self.cache_path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || metadata.is_dir())
        {
            return Err(Kind::UnsafeCache);
        }
        let bytes = serde_json::to_vec_pretty(&CacheDocument {
            version: CACHE_VERSION,
            models: models.to_vec(),
        })
        .map_err(|_| Kind::InvalidSchema)?;
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(Kind::Oversized);
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

impl FetchError {
    fn into_kind(self) -> Kind {
        match self {
            Self::Unauthorized => Kind::Network,
            Self::Other(kind) => kind,
        }
    }
}

/// Parses either OpenCode's Console Model List (`/console/api/v1/models`,
/// extracting the `opencode-go` provider) or the OpenAI-shaped Go inference
/// list (`/inference/go/v1/models` and `/zen/go/v1/models`).
pub fn parse_catalog_specs(bytes: &[u8]) -> Result<Vec<OpenCodeModelSpec>, CatalogError> {
    parse_specs_kind(bytes).map_err(CatalogError::new)
}

pub fn parse_catalog(bytes: &[u8]) -> Result<Vec<String>, CatalogError> {
    parse_catalog_specs(bytes).map(|models| models.into_iter().map(|m| m.id).collect())
}

fn parse_specs_kind(bytes: &[u8]) -> Result<Vec<OpenCodeModelSpec>, Kind> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(Kind::Oversized);
    }
    let root: Value = serde_json::from_slice(bytes).map_err(|_| Kind::InvalidSchema)?;
    let object = root.as_object().ok_or(Kind::InvalidSchema)?;

    if object.get("object").and_then(Value::as_str) == Some("list") {
        let data = object
            .get("data")
            .and_then(Value::as_array)
            .ok_or(Kind::InvalidSchema)?;
        return parse_model_entries(data.iter().map(|item| (None, item)));
    }

    if let Some(go_provider) = object.get("opencode-go").and_then(Value::as_object) {
        if let Some(models_map) = go_provider.get("models").and_then(Value::as_object) {
            return parse_model_entries(
                models_map
                    .iter()
                    .map(|(key, item)| (Some(key.as_str()), item)),
            );
        }
        if let Some(models_list) = go_provider.get("models").and_then(Value::as_array) {
            return parse_model_entries(models_list.iter().map(|item| (None, item)));
        }
    }

    Err(Kind::InvalidSchema)
}

fn parse_model_entries<'a>(
    entries: impl ExactSizeIterator<Item = (Option<&'a str>, &'a Value)>,
) -> Result<Vec<OpenCodeModelSpec>, Kind> {
    if entries.len() > MAX_CATALOG_ENTRIES {
        return Err(Kind::InvalidSchema);
    }
    let mut seen = HashSet::with_capacity(entries.len());
    let mut specs = Vec::with_capacity(entries.len());
    for (fallback_id, item) in entries {
        let obj = item.as_object().ok_or(Kind::InvalidSchema)?;
        let id = obj
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .or(fallback_id)
            .ok_or(Kind::InvalidSchema)?;
        validate_model_id(id)?;
        if !seen.insert(id.to_owned()) {
            return Err(Kind::DuplicateModelId);
        }
        // Console docs note non-chat models (voice, image, decision) may appear
        // later; only chat models can drive the agent loop.
        if let Some(kind) = obj.get("type").and_then(Value::as_str) {
            if kind != "chat" {
                continue;
            }
        }
        specs.push(spec_from_json(id, obj));
    }
    if specs.is_empty() {
        return Err(Kind::InvalidSchema);
    }
    Ok(specs)
}

fn spec_from_json(id: &str, obj: &serde_json::Map<String, Value>) -> OpenCodeModelSpec {
    let bundled = open_code_model(id);

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| bundled.map(|model| model.name.to_owned()))
        .unwrap_or_else(|| id.to_owned());

    let (api, base_url) = parse_api_and_base_url(obj, bundled);

    let limit = obj.get("limit").and_then(Value::as_object);
    let context_window = limit
        .and_then(|limit| limit.get("context"))
        .and_then(Value::as_u64)
        .filter(|&tokens| tokens > 0)
        .or_else(|| bundled.and_then(|model| model.context_window));
    let max_output_tokens = limit
        .and_then(|limit| limit.get("output"))
        .and_then(Value::as_u64)
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|&tokens| tokens > 0)
        .or_else(|| bundled.and_then(|model| model.max_output_tokens));

    let accepts_images = obj
        .get("modalities")
        .and_then(|modalities| modalities.get("input"))
        .and_then(Value::as_array)
        .map(|inputs| {
            inputs.iter().filter_map(Value::as_str).any(|modality| {
                modality.eq_ignore_ascii_case("image")
                    || modality
                        .get(..6)
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
            })
        })
        .unwrap_or_else(|| bundled.is_some_and(|model| model.accepts_images));

    let reasoning_levels = if let Some(capabilities) = obj.get("capabilities").and_then(Value::as_object) {
        capabilities
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_array)
            .map(|levels| normalize_reasoning_levels(levels))
            .unwrap_or_default()
    } else {
        bundled
            .map(|model| {
                model
                    .reasoning_levels
                    .iter()
                    .map(|level| (*level).to_owned())
                    .collect()
            })
            .unwrap_or_default()
    };

    OpenCodeModelSpec {
        id: id.to_owned(),
        name,
        api,
        context_window,
        max_output_tokens,
        accepts_images,
        reasoning_levels,
        base_url,
    }
}

fn parse_api_and_base_url(
    obj: &serde_json::Map<String, Value>,
    bundled: Option<&slim_core::provider::OpenCodeModel>,
) -> (OpenCodeApi, Option<String>) {
    let api_map = obj.get("api").and_then(Value::as_object);
    let recommended = obj.get("recommended_api").and_then(Value::as_str);

    let trusted_base = |key: &str| -> Option<String> {
        api_map
            .and_then(|map| map.get(key))
            .and_then(|entry| entry.get("base_url"))
            .and_then(Value::as_str)
            .filter(|url| is_trusted_opencode_base_url(url))
            .map(str::to_owned)
    };

    match recommended {
        Some("messages") => return (OpenCodeApi::AnthropicMessages, trusted_base("messages")),
        Some("responses") => return (OpenCodeApi::Responses, trusted_base("responses")),
        Some("chat-completions") => {
            return (OpenCodeApi::ChatCompletions, trusted_base("chat-completions"))
        }
        _ => {}
    }

    if let Some(map) = api_map {
        if map.contains_key("messages") {
            return (OpenCodeApi::AnthropicMessages, trusted_base("messages"));
        }
        if map.contains_key("responses") {
            return (OpenCodeApi::Responses, trusted_base("responses"));
        }
        if map.contains_key("chat-completions") {
            return (OpenCodeApi::ChatCompletions, trusted_base("chat-completions"));
        }
        // Per OpenCode Console docs, every Go text model (including Gemini
        // models whose native entry is `generate-content`/`interactions`) is
        // also served via OpenAI Chat Completions with gateway translation.
        return (
            OpenCodeApi::ChatCompletions,
            Some(OPENCODE_GO_OPENAI_BASE_URL.to_owned()),
        );
    }

    (
        bundled
            .map(|model| model.api)
            .unwrap_or(OpenCodeApi::ChatCompletions),
        None,
    )
}

fn normalize_reasoning_levels(levels: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for level in levels.iter().filter_map(Value::as_str) {
        let lower = level.trim().to_ascii_lowercase();
        if let Some(&canonical) = SUPPORTED_REASONING_LEVELS
            .iter()
            .find(|&&supported| supported == lower)
        {
            if !out.iter().any(|existing| existing == canonical) {
                out.push(canonical.to_owned());
            }
        }
    }
    out
}

fn read_cache_file(path: &Path) -> Result<Vec<OpenCodeModelSpec>, Kind> {
    let metadata = fs::symlink_metadata(path).map_err(|_| Kind::CacheIo)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_CATALOG_BYTES as u64
    {
        return Err(Kind::UnsafeCache);
    }
    let bytes = fs::read(path).map_err(|_| Kind::CacheIo)?;
    let document: CacheDocument =
        serde_json::from_slice(&bytes).map_err(|_| Kind::InvalidSchema)?;
    if document.version != CACHE_VERSION || document.models.is_empty() {
        return Err(Kind::InvalidSchema);
    }
    validate_cached_specs(&document.models)?;
    Ok(document.models)
}

fn validate_cached_specs(models: &[OpenCodeModelSpec]) -> Result<(), Kind> {
    if models.is_empty() || models.len() > MAX_CATALOG_ENTRIES {
        return Err(Kind::InvalidSchema);
    }
    let mut unique = HashSet::with_capacity(models.len());
    for model in models {
        validate_model_id(&model.id)?;
        if let Some(url) = &model.base_url {
            if !is_trusted_opencode_base_url(url) {
                return Err(Kind::InvalidSchema);
            }
        }
        if !unique.insert(model.id.as_str()) {
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
