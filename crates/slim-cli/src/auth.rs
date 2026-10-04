use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use slim_core::provider::ProviderKind;

const MAX_AUTH_FILE_BYTES: usize = 1024 * 1024;

/// The only supported auth-file format. Example (keys are placeholders):
///
/// ```json
/// {
///   "version": 1,
///   "providers": {
///     "openai-compatible": { "api_key": "sk-example" },
///     "anthropic": { "api_key": "sk-ant-example" }
///   }
/// }
/// ```
///
/// `SLIM_API_KEY` and the provider-specific environment variable always win
/// over this file. Explicit credential saves preserve sibling providers and
/// record the selected method in the provider entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthError {
    InvalidPath,
    Read,
    MalformedJson,
    UnsupportedVersion,
    InvalidSchema,
    UnsafePermissions,
    Write,
    Locked,
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidPath => "auth file path is invalid",
            Self::Read => "auth file could not be read",
            Self::MalformedJson => "auth file contains malformed JSON",
            Self::UnsupportedVersion => "auth file uses an unsupported schema version",
            Self::InvalidSchema => "auth file has an invalid schema",
            Self::UnsafePermissions => "auth file permissions could not be secured",
            Self::Write => "auth file could not be written",
            Self::Locked => "auth file is locked by another Slim process",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for AuthError {}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthDocument {
    version: u32,
    #[serde(default)]
    active_provider: Option<String>,
    providers: AuthProviders,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthProviders {
    #[serde(
        rename = "openai-compatible",
        alias = "openai_compatible",
        alias = "openai"
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    openai_compatible: Option<AuthProvider>,
    #[serde(
        rename = "openai-codex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    openai_codex: Option<AuthProvider>,
    #[serde(
        rename = "opencode-go",
        alias = "opencode_go",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    opencode_go: Option<AuthProvider>,
    #[serde(
        rename = "opencode-zen",
        alias = "opencode_zen",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    opencode_zen: Option<AuthProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anthropic: Option<AuthProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    xai: Option<AuthProvider>,
    #[serde(
        rename = "clinepass",
        alias = "cline-pass",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    clinepass: Option<AuthProvider>,
    #[serde(
        rename = "command-code",
        alias = "commandcode",
        alias = "command_code",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    command_code: Option<AuthProvider>,
    /// Legacy controller credential. Slim no longer reads or writes it, but the
    /// entry is preserved so existing auth files keep parsing under
    /// `deny_unknown_fields` and no stored model credential is lost.
    #[serde(rename = "typesafe", default, skip_serializing_if = "Option::is_none")]
    legacy_controller: Option<AuthProvider>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthProvider {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oauth: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "PreferredAuthMethodConfig::is_unset")]
    preferred_method: PreferredAuthMethodConfig,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum PreferredAuthMethod {
    #[serde(rename = "oauth")]
    OAuth,
    #[serde(rename = "api_key")]
    ApiKey,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PreferredAuthMethodConfig {
    #[default]
    Unset,
    Selected(PreferredAuthMethod),
}

impl PreferredAuthMethodConfig {
    fn is_unset(&self) -> bool {
        matches!(self, Self::Unset)
    }

    fn as_option(self) -> Option<PreferredAuthMethod> {
        match self {
            Self::Unset => None,
            Self::Selected(method) => Some(method),
        }
    }
}

impl<'de> Deserialize<'de> for PreferredAuthMethodConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "oauth" => Ok(Self::Selected(PreferredAuthMethod::OAuth)),
            "api_key" => Ok(Self::Selected(PreferredAuthMethod::ApiKey)),
            _ => Err(serde::de::Error::unknown_variant(
                &value,
                &["oauth", "api_key"],
            )),
        }
    }
}

impl Serialize for PreferredAuthMethodConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Unset => serializer.serialize_none(),
            Self::Selected(method) => method.serialize(serializer),
        }
    }
}

mod secret_debug_contract {
    use super::*;

    struct DebugMarker;

    trait DebugImplementation<A> {
        fn probe() {}
    }

    impl<T: ?Sized> DebugImplementation<()> for T {}

    impl<T: ?Sized + fmt::Debug> DebugImplementation<DebugMarker> for T {}

    const _: fn() = || {
        let _ = <AuthDocument as DebugImplementation<_>>::probe;
        let _ = <AuthProviders as DebugImplementation<_>>::probe;
        let _ = <AuthProvider as DebugImplementation<_>>::probe;
    };
}

/// Resolved provider access token plus optional OAuth metadata.
///
/// Environment variables and `api_key` in `auth.json` are not OAuth.
/// TUI `/login` persists `providers.<kind>.oauth`; that path sets `oauth`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCredential {
    pub access: String,
    pub account_id: Option<String>,
    pub oauth: bool,
}

/// Resolve a provider key using environment variables first, then auth.json.
pub fn resolve_api_key(kind: ProviderKind) -> Result<Option<String>, AuthError> {
    Ok(resolve_provider_credential(kind)?.map(|credential| credential.access))
}

/// Resolve access, optional account id, and whether the token came from TUI OAuth.
///
/// Precedence: `SLIM_API_KEY` / provider env → explicit file method, or the
/// legacy file order `oauth` before `api_key` when no method is recorded.
pub fn resolve_provider_credential(
    kind: ProviderKind,
) -> Result<Option<ProviderCredential>, AuthError> {
    let provider_variable = match kind {
        ProviderKind::OpenAiCompatible => "OPENAI_API_KEY",
        ProviderKind::OpenAiCodex => "CODEX_ACCESS_TOKEN",
        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
        ProviderKind::OpenCodeGo => "OPENCODE_API_KEY",
        // The Zen account key is the same OPENCODE_API_KEY issued for Go.
        ProviderKind::OpenCodeZen => "OPENCODE_API_KEY",
        ProviderKind::ClinePass => "CLINEPASS_API_KEY",
        ProviderKind::CommandCode => "COMMANDCODE_API_KEY",
        ProviderKind::Xai => "XAI_API_KEY",
    };
    if let Some(value) = non_empty_environment_value("SLIM_API_KEY") {
        return Ok(Some(env_credential(value)));
    }
    if let Some(value) = non_empty_environment_value(provider_variable) {
        return Ok(Some(env_credential(value)));
    }
    if kind == ProviderKind::CommandCode {
        if let Some(value) = non_empty_environment_value("CMD_API_KEY") {
            return Ok(Some(env_credential(value)));
        }
    }

    let Some(path) = auth_file_path()? else {
        return Ok(None);
    };
    load_auth_credential(&path, kind)
}

fn env_credential(access: String) -> ProviderCredential {
    ProviderCredential {
        access,
        account_id: None,
        oauth: false,
    }
}

/// Load one provider key from an existing auth file. This is public so the
/// offline contract tests can exercise file loading without making requests.
pub fn load_auth_file(path: &Path, kind: ProviderKind) -> Result<Option<String>, AuthError> {
    Ok(load_auth_credential(path, kind)?.map(|credential| credential.access))
}

/// Load `api_key` or TUI `oauth` from an existing auth file.
pub fn load_auth_credential(
    path: &Path,
    kind: ProviderKind,
) -> Result<Option<ProviderCredential>, AuthError> {
    let Some(document) = read_auth_document(path)? else {
        return Ok(None);
    };
    let provider = match kind {
        ProviderKind::OpenAiCompatible => document.providers.openai_compatible,
        ProviderKind::OpenAiCodex => document.providers.openai_codex,
        ProviderKind::Anthropic => document.providers.anthropic,
        ProviderKind::OpenCodeGo => document.providers.opencode_go,
        ProviderKind::OpenCodeZen => document.providers.opencode_zen,
        ProviderKind::ClinePass => document.providers.clinepass,
        ProviderKind::CommandCode => document.providers.command_code,
        ProviderKind::Xai => document.providers.xai,
    };
    let Some(provider) = provider else {
        return Ok(None);
    };
    match provider.preferred_method.as_option() {
        Some(PreferredAuthMethod::OAuth) => auth_oauth_credential(provider.oauth),
        Some(PreferredAuthMethod::ApiKey) => auth_api_key_credential(provider.api_key),
        None => auth_oauth_credential(provider.oauth).and_then(|credential| match credential {
            Some(credential) => Ok(Some(credential)),
            None => auth_api_key_credential(provider.api_key),
        }),
    }
}

fn auth_oauth_credential(
    oauth_value: Option<serde_json::Value>,
) -> Result<Option<ProviderCredential>, AuthError> {
    let Some(oauth_value) = oauth_value else {
        return Ok(None);
    };
    let oauth: crate::oauth::OAuthCredential =
        serde_json::from_value(oauth_value).map_err(|_| AuthError::InvalidSchema)?;
    if oauth.access.trim().is_empty() {
        return Err(AuthError::InvalidSchema);
    }
    Ok(Some(ProviderCredential {
        access: oauth.access,
        account_id: oauth.account_id,
        oauth: true,
    }))
}

fn auth_api_key_credential(key: Option<String>) -> Result<Option<ProviderCredential>, AuthError> {
    let Some(key) = key else {
        return Ok(None);
    };
    if key.trim().is_empty() {
        return Err(AuthError::InvalidSchema);
    }
    Ok(Some(ProviderCredential {
        access: key,
        account_id: None,
        oauth: false,
    }))
}

static AUTH_TEMP_NONCE: AtomicU64 = AtomicU64::new(1);

/// Atomically persist one provider API key while preserving sibling entries.
pub fn save_api_key_file(path: &Path, kind: ProviderKind, api_key: &str) -> Result<(), AuthError> {
    if api_key.trim().is_empty() || api_key.chars().count() > 4_096 {
        return Err(AuthError::InvalidSchema);
    }
    update_auth_file(path, |document| {
        let provider =
            provider_slot_mut(&mut document.providers, kind).get_or_insert_with(|| AuthProvider {
                api_key: None,
                oauth: None,
                preferred_method: PreferredAuthMethodConfig::Unset,
            });
        provider.api_key = Some(api_key.to_owned());
        provider.preferred_method =
            PreferredAuthMethodConfig::Selected(PreferredAuthMethod::ApiKey);
        document.active_provider = Some(provider_name(kind).to_owned());
    })
}

pub fn save_api_key(kind: ProviderKind, api_key: &str) -> Result<(), AuthError> {
    let path = auth_file_path()?.ok_or(AuthError::InvalidPath)?;
    save_api_key_file(&path, kind, api_key)
}

pub fn delete_api_key_file(path: &Path, kind: ProviderKind) -> Result<(), AuthError> {
    if !path.exists() {
        return Ok(());
    }
    update_auth_file(path, |document| {
        let provider_slot = provider_slot_mut(&mut document.providers, kind);
        if let Some(provider) = provider_slot.as_mut() {
            provider.api_key = None;
        }
        if provider_slot.as_ref().is_some_and(|provider| {
            provider.oauth.is_none() && provider.preferred_method.is_unset()
        }) {
            *provider_slot = None;
        }
        let selected_credential_exists = provider_slot
            .as_ref()
            .is_some_and(provider_has_selected_credential);
        if !selected_credential_exists
            && document.active_provider.as_deref() == Some(provider_name(kind))
        {
            document.active_provider = None;
        }
    })
}

pub fn delete_api_key(kind: ProviderKind) -> Result<(), AuthError> {
    let Some(path) = auth_file_path()? else {
        return Ok(());
    };
    delete_api_key_file(&path, kind)
}

/// Reads and validates the auth document without choosing a provider slot.
/// Returns `Ok(None)` when the file does not exist yet.
fn read_auth_document(path: &Path) -> Result<Option<AuthDocument>, AuthError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(AuthError::Read),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AuthError::InvalidPath);
    }
    let contents = secure_auth_file(path)?;
    let document: AuthDocument =
        serde_json::from_slice(&contents).map_err(|error| match error.classify() {
            serde_json::error::Category::Data | serde_json::error::Category::Eof => {
                AuthError::InvalidSchema
            }
            serde_json::error::Category::Syntax => AuthError::MalformedJson,
            serde_json::error::Category::Io => AuthError::Read,
        })?;
    if document.version != 1 {
        return Err(AuthError::UnsupportedVersion);
    }
    Ok(Some(document))
}

pub(crate) fn validate_auth_document_value(value: &serde_json::Value) -> Result<(), AuthError> {
    let document: AuthDocument =
        serde_json::from_value(value.clone()).map_err(|error| match error.classify() {
            serde_json::error::Category::Data | serde_json::error::Category::Eof => {
                AuthError::InvalidSchema
            }
            serde_json::error::Category::Syntax => AuthError::MalformedJson,
            serde_json::error::Category::Io => AuthError::Read,
        })?;
    if document.version != 1 {
        return Err(AuthError::UnsupportedVersion);
    }
    Ok(())
}

fn provider_name(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAiCompatible => "openai-compatible",
        ProviderKind::OpenAiCodex => "openai-codex",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenCodeGo => "opencode-go",
        ProviderKind::OpenCodeZen => "opencode-zen",
        ProviderKind::ClinePass => "clinepass",
        ProviderKind::CommandCode => "command-code",
        ProviderKind::Xai => "xai",
    }
}

fn provider_slot_mut(
    providers: &mut AuthProviders,
    kind: ProviderKind,
) -> &mut Option<AuthProvider> {
    match kind {
        ProviderKind::OpenAiCompatible => &mut providers.openai_compatible,
        ProviderKind::OpenAiCodex => &mut providers.openai_codex,
        ProviderKind::Anthropic => &mut providers.anthropic,
        ProviderKind::OpenCodeGo => &mut providers.opencode_go,
        ProviderKind::OpenCodeZen => &mut providers.opencode_zen,
        ProviderKind::ClinePass => &mut providers.clinepass,
        ProviderKind::CommandCode => &mut providers.command_code,
        ProviderKind::Xai => &mut providers.xai,
    }
}

fn provider_has_selected_credential(provider: &AuthProvider) -> bool {
    match provider.preferred_method.as_option() {
        Some(PreferredAuthMethod::OAuth) => provider.oauth.is_some(),
        Some(PreferredAuthMethod::ApiKey) => provider.api_key.is_some(),
        None => provider.oauth.is_some() || provider.api_key.is_some(),
    }
}

fn update_auth_file(path: &Path, update: impl FnOnce(&mut AuthDocument)) -> Result<(), AuthError> {
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || metadata.is_dir())
    {
        return Err(AuthError::InvalidPath);
    }
    let parent = path.parent().ok_or(AuthError::InvalidPath)?;
    fs::create_dir_all(parent).map_err(|_| AuthError::Write)?;
    if fs::symlink_metadata(parent)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(AuthError::InvalidPath);
    }

    let _guard = lock_auth_store(path)?;

    let mut document = if path.exists() {
        parse_auth_document(&secure_auth_file(path)?)?
    } else {
        AuthDocument {
            version: 1,
            active_provider: None,
            providers: AuthProviders::default(),
        }
    };
    if document.version != 1 {
        return Err(AuthError::UnsupportedVersion);
    }
    update(&mut document);
    let bytes = serde_json::to_vec_pretty(&document).map_err(|_| AuthError::InvalidSchema)?;
    let temporary = parent.join(format!(
        ".auth-{}-{}.tmp",
        std::process::id(),
        AUTH_TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = create_secure_auth_file(&temporary)?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| AuthError::Write)?;
        drop(file);
        replace_auth_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn parse_auth_document(contents: &[u8]) -> Result<AuthDocument, AuthError> {
    serde_json::from_slice(contents).map_err(|error| match error.classify() {
        serde_json::error::Category::Data | serde_json::error::Category::Eof => {
            AuthError::InvalidSchema
        }
        serde_json::error::Category::Syntax => AuthError::MalformedJson,
        serde_json::error::Category::Io => AuthError::Read,
    })
}

pub(crate) struct AuthStoreLock {
    #[cfg(windows)]
    _file: File,
}

pub(crate) struct OAuthRefreshLock {
    #[cfg(windows)]
    _file: File,
}

pub(crate) enum OAuthRefreshLockError {
    Cancelled,
    Auth(AuthError),
}

impl From<AuthError> for OAuthRefreshLockError {
    fn from(error: AuthError) -> Self {
        Self::Auth(error)
    }
}

pub(crate) fn auth_lock_path(auth_file: &Path) -> Result<PathBuf, AuthError> {
    Ok(auth_file
        .parent()
        .ok_or(AuthError::InvalidPath)?
        .join(".auth.lock"))
}

pub(crate) fn lock_auth_store(auth_file: &Path) -> Result<AuthStoreLock, AuthError> {
    lock_auth_store_until(auth_file, Instant::now() + Duration::from_secs(30), || {
        false
    })
}

pub(crate) fn lock_auth_store_until(
    auth_file: &Path,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<AuthStoreLock, AuthError> {
    let parent = auth_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|_| AuthError::Write)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let lock_path = auth_lock_path(auth_file)?;
        loop {
            if cancelled() || Instant::now() >= deadline {
                return Err(AuthError::Locked);
            }
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(&lock_path)
            {
                Ok(file) => {
                    if cancelled() || Instant::now() >= deadline {
                        drop(file);
                        return Err(AuthError::Locked);
                    }
                    return Ok(AuthStoreLock { _file: file });
                }
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = parent;
        if cancelled() || Instant::now() >= deadline {
            Err(AuthError::Locked)
        } else {
            Ok(AuthStoreLock {})
        }
    }
}

pub(crate) fn lock_oauth_refresh(
    auth_file: &Path,
    provider: &str,
    cancelled: impl Fn() -> bool,
) -> Result<OAuthRefreshLock, OAuthRefreshLockError> {
    if cancelled() {
        return Err(OAuthRefreshLockError::Cancelled);
    }
    let lock_path = oauth_refresh_lock_path(auth_file, provider)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if cancelled() {
                return Err(OAuthRefreshLockError::Cancelled);
            }
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(&lock_path)
            {
                Ok(file) => {
                    if cancelled() {
                        drop(file);
                        return Err(OAuthRefreshLockError::Cancelled);
                    }
                    return Ok(OAuthRefreshLock { _file: file });
                }
                Err(_) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return Err(OAuthRefreshLockError::Auth(AuthError::Locked)),
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = lock_path;
        if cancelled() {
            return Err(OAuthRefreshLockError::Cancelled);
        }
        Ok(OAuthRefreshLock {})
    }
}

fn oauth_refresh_lock_path(auth_file: &Path, provider: &str) -> Result<PathBuf, AuthError> {
    if !matches!(provider, "anthropic" | "openai-codex" | "xai") {
        return Err(AuthError::InvalidPath);
    }
    let parent = auth_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|_| AuthError::Write)?;
    let canonical_parent = fs::canonicalize(parent).map_err(|_| AuthError::InvalidPath)?;
    let file_name = auth_file.file_name().ok_or(AuthError::InvalidPath)?;
    let mut identity = canonical_parent
        .join(file_name)
        .to_string_lossy()
        .into_owned();
    if cfg!(windows) {
        identity = identity.replace('/', "\\").to_lowercase();
    }
    identity.push('\0');
    identity.push_str(provider);
    let digest = Sha256::digest(identity.as_bytes());
    Ok(canonical_parent.join(format!(".oauth-refresh-{digest:x}.lock")))
}

#[cfg(windows)]
fn replace_auth_file(source: &Path, destination: &Path) -> Result<(), AuthError> {
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
        Err(AuthError::Write)
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_auth_file(source: &Path, destination: &Path) -> Result<(), AuthError> {
    fs::rename(source, destination).map_err(|_| AuthError::Write)
}

fn non_empty_environment_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

pub(crate) fn auth_file_path() -> Result<Option<PathBuf>, AuthError> {
    if let Some(path) = std::env::var_os("SLIM_AUTH_FILE") {
        if path.is_empty() {
            return Err(AuthError::InvalidPath);
        }
        return Ok(Some(PathBuf::from(path)));
    }
    let Some(profile) = std::env::var_os("USERPROFILE") else {
        return Ok(None);
    };
    if profile.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(profile).join(".slim").join("auth.json")))
}

#[cfg(windows)]
pub(crate) fn secure_auth_file(path: &Path) -> Result<Vec<u8>, AuthError> {
    NativeAuthFile::open(path)?.secure_and_read()
}

#[cfg(not(windows))]
pub(crate) fn secure_auth_file(_path: &Path) -> Result<Vec<u8>, AuthError> {
    Err(AuthError::UnsafePermissions)
}

#[cfg(windows)]
pub(crate) fn create_secure_auth_file(path: &Path) -> Result<File, AuthError> {
    native_acl::NativeAuthFile::create_secure(path)
}

#[cfg(not(windows))]
pub(crate) fn create_secure_auth_file(_path: &Path) -> Result<File, AuthError> {
    Err(AuthError::UnsafePermissions)
}

pub fn redact(input: &str) -> String {
    slim_core::redact_credentials(input)
}

/// `redact` plus literal secret values, longest first — the same discipline as
/// the runtime's `normalize_sensitive_values`. Replacing in arbitrary order
/// lets a shorter secret that overlaps a longer one consume its head and leak
/// the remaining tail.
pub(crate) fn redact_with_secrets(input: &str, secrets: &[String]) -> String {
    let mut secrets: Vec<&String> = secrets.iter().filter(|value| !value.is_empty()).collect();
    secrets.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    secrets.dedup();
    let mut text = redact(input);
    for secret in secrets {
        text = text.replace(secret.as_str(), "[REDACTED]");
    }
    text
}

/// Reads one owner-only JSON document stored beside the credentials (MCP
/// trust decisions, MCP OAuth tokens). Same path and permission discipline as
/// `auth.json`: symlinks and non-files are refused and the file's ACL is
/// verified before its bytes are trusted. `Ok(None)` when it does not exist.
pub(crate) fn read_secure_json(path: &Path) -> Result<Option<serde_json::Value>, AuthError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(AuthError::Read),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AuthError::InvalidPath);
    }
    let contents = secure_auth_file(path)?;
    serde_json::from_slice(&contents)
        .map(Some)
        .map_err(|error| match error.classify() {
            serde_json::error::Category::Data | serde_json::error::Category::Eof => {
                AuthError::InvalidSchema
            }
            serde_json::error::Category::Syntax => AuthError::MalformedJson,
            serde_json::error::Category::Io => AuthError::Read,
        })
}

/// Read-modify-write of a secure JSON document under the auth store lock,
/// replacing it atomically through an owner-only temporary file.
pub(crate) fn update_secure_json(
    path: &Path,
    update: impl FnOnce(Option<serde_json::Value>) -> Result<serde_json::Value, AuthError>,
) -> Result<(), AuthError> {
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || metadata.is_dir())
    {
        return Err(AuthError::InvalidPath);
    }
    let parent = path.parent().ok_or(AuthError::InvalidPath)?;
    fs::create_dir_all(parent).map_err(|_| AuthError::Write)?;
    if fs::symlink_metadata(parent)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(AuthError::InvalidPath);
    }
    let _guard = lock_auth_store(path)?;
    let current = read_secure_json(path)?;
    let document = update(current)?;
    let bytes = serde_json::to_vec_pretty(&document).map_err(|_| AuthError::InvalidSchema)?;
    if bytes.len() > MAX_AUTH_FILE_BYTES {
        return Err(AuthError::InvalidSchema);
    }
    let temporary = parent.join(format!(
        ".secure-json-{}-{}.tmp",
        std::process::id(),
        AUTH_TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = create_secure_auth_file(&temporary)?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| AuthError::Write)?;
        drop(file);
        replace_auth_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(all(test, windows))]
mod secure_creation_tests {
    use super::*;

    #[test]
    fn secure_temp_handle_blocks_path_replacement_while_writing() {
        let root =
            std::env::temp_dir().join(format!("slim-secure-auth-create-{}", std::process::id()));
        fs::create_dir_all(&root).expect("root");
        let path = root.join("auth.tmp");
        let mut file = create_secure_auth_file(&path).expect("secure create");
        file.write_all(b"secret").expect("write");
        assert!(fs::remove_file(&path).is_err());
        drop(file);
        fs::remove_dir_all(root).expect("cleanup");
    }
}

#[cfg(windows)]
mod native_acl {
    use super::{AuthError, MAX_AUTH_FILE_BYTES};
    use std::fs::File;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER, GENERIC_READ,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W, SET_ACCESS,
        SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        AclSizeInformation, CreateWellKnownSid, EqualSid, GetAce, GetAclInformation, GetLengthSid,
        GetSecurityDescriptorControl, GetTokenInformation, IsValidAcl, IsValidSid, TokenUser,
        WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACE_HEADER,
        ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, NO_INHERITANCE,
        PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_MAX_SID_SIZE, SE_DACL_PRESENT,
        SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileAttributeTagInfo, GetFileInformationByHandleEx, GetFileSizeEx,
        GetFileType, ReadFile, CREATE_NEW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_TYPE_DISK, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct NativeHandle(HANDLE);

    impl Drop for NativeHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    struct LocalAllocation(*mut core::ffi::c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = LocalFree(self.0);
                }
            }
        }
    }

    #[derive(Clone)]
    struct Sid(Vec<u8>);

    impl Sid {
        fn as_ptr(&self) -> *mut core::ffi::c_void {
            self.0.as_ptr() as *mut core::ffi::c_void
        }
    }

    struct AllowedSid {
        sid: Sid,
        trustee_type: i32,
    }

    pub(super) struct NativeAuthFile {
        handle: NativeHandle,
    }

    impl NativeAuthFile {
        pub(super) fn create_secure(path: &Path) -> Result<File, AuthError> {
            let path_wide = wide_path(path)?;
            let handle = unsafe {
                CreateFileW(
                    path_wide.as_ptr(),
                    GENERIC_READ
                        | GENERIC_WRITE
                        | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
                    0,
                    null(),
                    CREATE_NEW,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE || handle.is_null() {
                return Err(AuthError::Write);
            }
            let file = Self {
                handle: NativeHandle(handle),
            };
            file.ensure_regular_non_reparse_file()?;
            let allowed = allowed_sids(file.handle.0)?;
            install_dacl(file.handle.0, &allowed)?;
            verify_dacl(file.handle.0, &allowed)?;
            let handle = file.handle.0;
            std::mem::forget(file);
            Ok(unsafe { File::from_raw_handle(handle) })
        }

        pub(super) fn open(path: &Path) -> Result<Self, AuthError> {
            let path_wide = wide_path(path)?;
            let handle = unsafe {
                CreateFileW(
                    path_wide.as_ptr(),
                    GENERIC_READ | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
                    FILE_SHARE_READ,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE || handle.is_null() {
                return Err(AuthError::UnsafePermissions);
            }
            let file = Self {
                handle: NativeHandle(handle),
            };
            file.ensure_regular_non_reparse_file()?;
            Ok(file)
        }

        pub(super) fn secure_and_read(self) -> Result<Vec<u8>, AuthError> {
            let allowed = allowed_sids(self.handle.0)?;

            install_dacl(self.handle.0, &allowed)?;
            verify_dacl(self.handle.0, &allowed)?;
            read_contents(self.handle.0)
        }

        fn ensure_regular_non_reparse_file(&self) -> Result<(), AuthError> {
            let mut info = FILE_ATTRIBUTE_TAG_INFO {
                FileAttributes: 0,
                ReparseTag: 0,
            };
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    self.handle.0,
                    FileAttributeTagInfo,
                    &mut info as *mut _ as *mut core::ffi::c_void,
                    size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
                )
            };
            if ok == 0
                || unsafe { GetFileType(self.handle.0) } != FILE_TYPE_DISK
                || info.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT)
                    != 0
            {
                return Err(AuthError::InvalidPath);
            }
            Ok(())
        }
    }

    fn wide_path(path: &Path) -> Result<Vec<u16>, AuthError> {
        if path.as_os_str().is_empty() {
            return Err(AuthError::InvalidPath);
        }
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        if wide.len() > u32::MAX as usize || wide[..wide.len() - 1].contains(&0) {
            return Err(AuthError::InvalidPath);
        }
        Ok(wide)
    }

    fn current_user_sid() -> Result<Sid, AuthError> {
        let mut token = null_mut();
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 || token.is_null() {
            return Err(AuthError::UnsafePermissions);
        }
        let token = NativeHandle(token);
        let mut required = 0u32;
        let first =
            unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required) };
        if first != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || required == 0 {
            return Err(AuthError::UnsafePermissions);
        }
        let mut buffer = vec![0u8; required as usize];
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr() as *mut core::ffi::c_void,
                required,
                &mut required,
            )
        };
        if ok == 0 {
            return Err(AuthError::UnsafePermissions);
        }
        let token_user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        copy_sid(token_user.User.Sid)
    }

    fn well_known_sid(
        kind: windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE,
    ) -> Result<Sid, AuthError> {
        let mut buffer = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut size = buffer.len() as u32;
        let ok = unsafe {
            CreateWellKnownSid(kind, null_mut(), buffer.as_mut_ptr() as *mut _, &mut size)
        };
        if ok == 0 || size == 0 || size > buffer.len() as u32 {
            return Err(AuthError::UnsafePermissions);
        }
        buffer.truncate(size as usize);
        if unsafe { IsValidSid(buffer.as_mut_ptr() as *mut _) } == 0 {
            return Err(AuthError::UnsafePermissions);
        }
        Ok(Sid(buffer))
    }

    fn copy_sid(sid: *mut core::ffi::c_void) -> Result<Sid, AuthError> {
        if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
            return Err(AuthError::UnsafePermissions);
        }
        let length = unsafe { GetLengthSid(sid) } as usize;
        if length == 0 || length > SECURITY_MAX_SID_SIZE as usize {
            return Err(AuthError::UnsafePermissions);
        }
        let bytes = unsafe { std::slice::from_raw_parts(sid as *const u8, length) }.to_vec();
        Ok(Sid(bytes))
    }

    fn owner_sid(handle: HANDLE) -> Result<Sid, AuthError> {
        let mut owner = null_mut();
        let mut descriptor = null_mut();
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        let _descriptor = LocalAllocation(descriptor);
        if status != 0 || owner.is_null() || descriptor.is_null() {
            return Err(AuthError::UnsafePermissions);
        }
        copy_sid(owner)
    }

    fn push_unique_sid(sids: &mut Vec<AllowedSid>, sid: Sid, trustee_type: i32) {
        if !sids
            .iter()
            .any(|existing| unsafe { EqualSid(existing.sid.as_ptr(), sid.as_ptr()) != 0 })
        {
            sids.push(AllowedSid { sid, trustee_type });
        }
    }

    fn allowed_sids(handle: HANDLE) -> Result<Vec<AllowedSid>, AuthError> {
        let current_user = current_user_sid()?;
        let owner = owner_sid(handle)?;
        let system = well_known_sid(WinLocalSystemSid)?;
        let administrators = well_known_sid(WinBuiltinAdministratorsSid)?;
        let mut allowed = Vec::with_capacity(4);
        push_unique_sid(&mut allowed, owner, TRUSTEE_IS_USER);
        push_unique_sid(&mut allowed, current_user, TRUSTEE_IS_USER);
        push_unique_sid(&mut allowed, system, TRUSTEE_IS_WELL_KNOWN_GROUP);
        push_unique_sid(&mut allowed, administrators, TRUSTEE_IS_WELL_KNOWN_GROUP);
        Ok(allowed)
    }

    fn trustee(sid: &Sid, trustee_type: i32) -> TRUSTEE_W {
        TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: trustee_type,
            ptstrName: sid.as_ptr() as *mut u16,
        }
    }

    fn install_dacl(handle: HANDLE, sids: &[AllowedSid]) -> Result<(), AuthError> {
        let entries: Vec<EXPLICIT_ACCESS_W> = sids
            .iter()
            .map(|allowed| EXPLICIT_ACCESS_W {
                grfAccessPermissions: FILE_ALL_ACCESS,
                grfAccessMode: SET_ACCESS,
                grfInheritance: NO_INHERITANCE,
                Trustee: trustee(&allowed.sid, allowed.trustee_type),
            })
            .collect();
        let mut acl = null_mut();
        let status =
            unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), null(), &mut acl) };
        let _acl = LocalAllocation(acl as *mut core::ffi::c_void);
        if status != 0 || acl.is_null() {
            return Err(AuthError::UnsafePermissions);
        }
        let status = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl,
                null_mut(),
            )
        };
        if status != 0 {
            return Err(AuthError::UnsafePermissions);
        }
        Ok(())
    }

    fn verify_dacl(handle: HANDLE, expected: &[AllowedSid]) -> Result<(), AuthError> {
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        let _descriptor = LocalAllocation(descriptor);
        if status != 0 || dacl.is_null() || descriptor.is_null() {
            return Err(AuthError::UnsafePermissions);
        }

        let mut control = 0u16;
        let mut revision = 0u32;
        let control_ok =
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
        if control_ok == 0
            || control & (SE_DACL_PRESENT | SE_DACL_PROTECTED)
                != (SE_DACL_PRESENT | SE_DACL_PROTECTED)
        {
            return Err(AuthError::UnsafePermissions);
        }

        let mut size_info = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        let acl_ok = unsafe {
            GetAclInformation(
                dacl,
                &mut size_info as *mut _ as *mut core::ffi::c_void,
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        if acl_ok == 0
            || unsafe { IsValidAcl(dacl) } == 0
            || size_info.AceCount as usize != expected.len()
        {
            return Err(AuthError::UnsafePermissions);
        }

        let mut matched = vec![false; expected.len()];
        for index in 0..size_info.AceCount {
            let mut ace = null_mut();
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
                return Err(AuthError::UnsafePermissions);
            }
            let header = unsafe { &*(ace as *const ACE_HEADER) };
            if header.AceType != 0 || header.AceFlags != 0 {
                return Err(AuthError::UnsafePermissions);
            }
            if (header.AceSize as usize) < size_of::<ACCESS_ALLOWED_ACE>() {
                return Err(AuthError::UnsafePermissions);
            }
            let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            if allowed.Mask != FILE_ALL_ACCESS {
                return Err(AuthError::UnsafePermissions);
            }
            let sid = &allowed.SidStart as *const u32 as *mut core::ffi::c_void;
            if unsafe { IsValidSid(sid) } == 0 {
                return Err(AuthError::UnsafePermissions);
            }
            let Some(expected_index) = expected
                .iter()
                .position(|candidate| unsafe { EqualSid(candidate.sid.as_ptr(), sid) != 0 })
            else {
                return Err(AuthError::UnsafePermissions);
            };
            if matched[expected_index] {
                return Err(AuthError::UnsafePermissions);
            }
            matched[expected_index] = true;
        }
        if matched.into_iter().all(|value| value) {
            Ok(())
        } else {
            Err(AuthError::UnsafePermissions)
        }
    }

    fn read_contents(handle: HANDLE) -> Result<Vec<u8>, AuthError> {
        let mut size = 0i64;
        if unsafe { GetFileSizeEx(handle, &mut size) } == 0
            || size < 0
            || size as u64 > MAX_AUTH_FILE_BYTES as u64
        {
            return Err(AuthError::Read);
        }
        let size = usize::try_from(size).map_err(|_| AuthError::Read)?;
        let mut contents = vec![0u8; size];
        let mut offset = 0usize;
        while offset < contents.len() {
            let requested = (contents.len() - offset).min(u32::MAX as usize) as u32;
            let mut read = 0u32;
            if unsafe {
                ReadFile(
                    handle,
                    contents[offset..].as_mut_ptr(),
                    requested,
                    &mut read,
                    null_mut(),
                )
            } == 0
                || read == 0
            {
                return Err(AuthError::Read);
            }
            offset = offset.checked_add(read as usize).ok_or(AuthError::Read)?;
        }
        let mut final_size = 0i64;
        if unsafe { GetFileSizeEx(handle, &mut final_size) } == 0 || final_size != size as i64 {
            return Err(AuthError::Read);
        }
        Ok(contents)
    }
}

#[cfg(windows)]
use native_acl::NativeAuthFile;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_with_secrets_replaces_longest_first_on_overlap() {
        // Shorter secret is a substring of the longer one: applied first it
        // would leave "123def" — the tail of the real secret — in the output.
        let secrets = vec!["abc".to_string(), "abc123def".to_string()];
        assert_eq!(
            redact_with_secrets("token abc123def here", &secrets),
            "token [REDACTED] here"
        );
    }

    #[test]
    fn redact_with_secrets_mid_overlap_leaks_no_fragments() {
        // "SECRET" overlaps the middle of the longer secret: applied first it
        // would leak both "XYZ-" and "-789" fragments.
        let secrets = vec!["SECRET".to_string(), "XYZ-SECRET-789".to_string()];
        let out = redact_with_secrets("key=XYZ-SECRET-789", &secrets);
        assert_eq!(out, "key=[REDACTED]");
        assert!(!out.contains("789"));
    }

    #[test]
    fn redact_with_secrets_ignores_empty_and_dedups() {
        let secrets = vec![String::new(), "dup".to_string(), "dup".to_string()];
        assert_eq!(redact_with_secrets("a dup b", &secrets), "a [REDACTED] b");
    }

    #[test]
    fn legacy_controller_entry_is_tolerated_and_never_becomes_active() {
        let document: AuthDocument = serde_json::from_str(
            r#"{"version":1,"active_provider":"opencode-go","providers":{"opencode-go":{"api_key":"model-fixture-key"},"typesafe":{"api_key":"legacy-fixture-key"}}}"#,
        )
        .expect("an auth file carrying the legacy controller entry must keep parsing");

        assert_eq!(document.active_provider.as_deref(), Some("opencode-go"));
        assert_eq!(
            document
                .providers
                .opencode_go
                .and_then(|provider| provider.api_key)
                .as_deref(),
            Some("model-fixture-key")
        );
    }
}
