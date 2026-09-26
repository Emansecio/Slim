use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use super::{OAuthCredential, OAuthError, OAuthProvider};
use crate::auth::{auth_file_path, create_secure_auth_file, secure_auth_file, PreferredAuthMethod};

static STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) struct StoreGuard {
    _lock: crate::auth::AuthStoreLock,
}

pub(crate) struct RefreshGuard {
    _lock: crate::auth::OAuthRefreshLock,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AuthEntrySnapshot {
    oauth: Option<Value>,
    api_key: Option<Value>,
    preferred_method: Option<PreferredAuthMethod>,
}

impl AuthEntrySnapshot {
    pub(crate) fn has_oauth(&self) -> bool {
        self.oauth.is_some()
    }

    pub(crate) fn oauth_matches(&self, credential: &OAuthCredential) -> bool {
        serde_json::to_value(credential).is_ok_and(|value| self.oauth.as_ref() == Some(&value))
    }

    pub(crate) fn selection_matches(&self, other: &Self) -> bool {
        self.api_key == other.api_key && self.preferred_method == other.preferred_method
    }

    pub(crate) fn prefers_api_key(&self) -> bool {
        self.preferred_method == Some(PreferredAuthMethod::ApiKey)
    }

    pub(crate) fn prefers_oauth(&self) -> bool {
        self.preferred_method == Some(PreferredAuthMethod::OAuth)
    }

    pub(crate) fn with_oauth(&self, credential: &OAuthCredential) -> Result<Self, OAuthError> {
        let mut snapshot = self.clone();
        snapshot.oauth = Some(
            serde_json::to_value(credential)
                .map_err(|_| OAuthError::Store("OAuth credential serialization failed".into()))?,
        );
        Ok(snapshot)
    }
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Clone, Debug)]
pub struct OAuthStore {
    path: PathBuf,
    #[cfg(test)]
    refresh_write_failure: Arc<AtomicBool>,
}

impl OAuthStore {
    pub fn default_path() -> Result<Self, OAuthError> {
        let path = auth_file_path()
            .map_err(|error| OAuthError::Store(error.to_string()))?
            .ok_or_else(|| OAuthError::Store("USERPROFILE is unavailable".into()))?;
        Ok(Self::at(path))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            #[cfg(test)]
            refresh_write_failure: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn active(&self) -> Result<Option<(OAuthProvider, OAuthCredential)>, OAuthError> {
        let document = self.read_document()?;
        let Some(provider) = document
            .get("active_provider")
            .and_then(Value::as_str)
            .and_then(parse_provider)
        else {
            return Ok(None);
        };
        if preferred_method(&document, provider)? == Some(PreferredAuthMethod::ApiKey) {
            return Ok(None);
        }
        self.credential_from(&document, provider)
            .transpose()
            .map(|credential| credential.map(|credential| (provider, credential)))
    }

    pub fn credential(
        &self,
        provider: OAuthProvider,
    ) -> Result<Option<OAuthCredential>, OAuthError> {
        let document = self.read_document()?;
        if preferred_method(&document, provider)? == Some(PreferredAuthMethod::ApiKey) {
            return Ok(None);
        }
        self.credential_from(&document, provider).transpose()
    }

    pub(crate) fn credential_snapshot(
        &self,
        provider: OAuthProvider,
    ) -> Result<(Option<OAuthCredential>, AuthEntrySnapshot), OAuthError> {
        let document = self.read_document()?;
        let entry = document.pointer(&format!("/providers/{}", provider.key()));
        let snapshot = AuthEntrySnapshot {
            oauth: entry.and_then(|entry| entry.get("oauth")).cloned(),
            api_key: entry.and_then(|entry| entry.get("api_key")).cloned(),
            preferred_method: preferred_method(&document, provider)?,
        };
        let credential = snapshot
            .oauth
            .clone()
            .filter(|_| !snapshot.prefers_api_key())
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| OAuthError::Store("OAuth credential schema is invalid".into()))?;
        Ok((credential, snapshot))
    }

    pub fn api_key(&self, provider: &str) -> Result<Option<String>, OAuthError> {
        validate_api_key_provider(provider)?;
        self.read_document()?
            .pointer(&format!("/providers/{provider}/api_key"))
            .map(|value| {
                value
                    .as_str()
                    .filter(|key| !key.trim().is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| OAuthError::Store("API key schema is invalid".into()))
            })
            .transpose()
    }

    pub fn save_api_key(&self, provider: &str, key: &str) -> Result<(), OAuthError> {
        validate_api_key_provider(provider)?;
        if key.trim().is_empty() {
            return Err(OAuthError::Store("API key cannot be empty".into()));
        }
        let _store_guard = self.lock_exclusive()?;
        let _guard = STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| OAuthError::Store("auth store lock poisoned".into()))?;
        let mut document = self.read_document()?;
        document["active_provider"] = Value::String(provider.into());
        let providers = document
            .get_mut("providers")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| OAuthError::Store("auth file providers must be an object".into()))?;
        let entry = providers.entry(provider).or_insert_with(|| json!({}));
        let entry = entry
            .as_object_mut()
            .ok_or_else(|| OAuthError::Store("provider auth entry must be an object".into()))?;
        entry.insert("api_key".into(), Value::String(key.into()));
        entry.insert("preferred_method".into(), Value::String("api_key".into()));
        self.write_document(&document)
    }

    pub fn activate_api_key(&self, provider: &str) -> Result<Option<String>, OAuthError> {
        validate_api_key_provider(provider)?;
        let _store_guard = self.lock_exclusive()?;
        let _guard = STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| OAuthError::Store("auth store lock poisoned".into()))?;
        let mut document = self.read_document()?;
        let Some(key) = document
            .pointer(&format!("/providers/{provider}/api_key"))
            .map(|value| {
                value
                    .as_str()
                    .filter(|key| !key.trim().is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| OAuthError::Store("API key schema is invalid".into()))
            })
            .transpose()?
        else {
            return Ok(None);
        };
        document["active_provider"] = Value::String(provider.into());
        document
            .pointer_mut(&format!("/providers/{provider}"))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| OAuthError::Store("provider auth entry must be an object".into()))?
            .insert("preferred_method".into(), Value::String("api_key".into()));
        self.write_document(&document)?;
        Ok(Some(key))
    }

    pub fn remove_api_key(&self, provider: &str) -> Result<(), OAuthError> {
        validate_api_key_provider(provider)?;
        let _store_guard = self.lock_exclusive()?;
        let _guard = STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| OAuthError::Store("auth store lock poisoned".into()))?;
        let mut document = self.read_document()?;
        if let Some(providers) = document.get_mut("providers").and_then(Value::as_object_mut) {
            let remove_entry = providers
                .get_mut(provider)
                .and_then(Value::as_object_mut)
                .is_some_and(|entry| {
                    entry.remove("api_key");
                    entry.is_empty()
                });
            if remove_entry {
                providers.remove(provider);
            }
        }
        if document.get("active_provider").and_then(Value::as_str) == Some(provider) {
            let entry = document.pointer(&format!("/providers/{provider}"));
            let has_selected_credential = match entry
                .and_then(|entry| entry.get("preferred_method"))
                .and_then(Value::as_str)
            {
                Some("oauth") => entry.and_then(|entry| entry.get("oauth")).is_some(),
                Some("api_key") => entry.and_then(|entry| entry.get("api_key")).is_some(),
                None => {
                    entry.and_then(|entry| entry.get("oauth")).is_some()
                        || entry.and_then(|entry| entry.get("api_key")).is_some()
                }
                Some(_) => false,
            };
            if !has_selected_credential {
                document
                    .as_object_mut()
                    .map(|object| object.remove("active_provider"));
            }
        }
        self.write_document(&document)
    }

    pub fn active_provider_key(&self) -> Result<Option<String>, OAuthError> {
        Ok(self
            .read_document()?
            .get("active_provider")
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    pub fn save(
        &self,
        provider: OAuthProvider,
        credential: &OAuthCredential,
    ) -> Result<(), OAuthError> {
        let _store_guard = self.lock_exclusive()?;
        self.save_locked(provider, credential)
    }

    pub(crate) fn save_locked(
        &self,
        provider: OAuthProvider,
        credential: &OAuthCredential,
    ) -> Result<(), OAuthError> {
        let _guard = STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| OAuthError::Store("auth store lock poisoned".into()))?;
        let mut document = self.read_document()?;
        document["active_provider"] = Value::String(provider.key().into());
        let providers = document
            .get_mut("providers")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| OAuthError::Store("auth file providers must be an object".into()))?;
        let entry = providers.entry(provider.key()).or_insert_with(|| json!({}));
        let entry = entry
            .as_object_mut()
            .ok_or_else(|| OAuthError::Store("provider auth entry must be an object".into()))?;
        entry.insert(
            "oauth".into(),
            serde_json::to_value(credential)
                .map_err(|_| OAuthError::Store("OAuth credential serialization failed".into()))?,
        );
        entry.insert("preferred_method".into(), Value::String("oauth".into()));
        self.write_document(&document)
    }

    pub(crate) fn persist_refresh_locked_if_unchanged(
        &self,
        provider: OAuthProvider,
        expected: &AuthEntrySnapshot,
        refreshed: &OAuthCredential,
    ) -> Result<bool, OAuthError> {
        let _guard = STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| OAuthError::Store("auth store lock poisoned".into()))?;
        if !expected.has_oauth() || expected.prefers_api_key() {
            return Ok(false);
        }
        let mut document = self.read_document()?;
        let entry = document.pointer(&format!("/providers/{}", provider.key()));
        let current = AuthEntrySnapshot {
            oauth: entry.and_then(|entry| entry.get("oauth")).cloned(),
            api_key: entry.and_then(|entry| entry.get("api_key")).cloned(),
            preferred_method: preferred_method(&document, provider)?,
        };
        if current != *expected {
            return Ok(false);
        }
        #[cfg(test)]
        if self.refresh_write_failure.load(Ordering::Acquire) {
            return Err(OAuthError::Store(
                "injected OAuth persistence failure".into(),
            ));
        }
        let entry = document
            .pointer_mut(&format!("/providers/{}", provider.key()))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                OAuthError::Store("auth provider entry changed during refresh".into())
            })?;
        entry.insert(
            "oauth".into(),
            serde_json::to_value(refreshed)
                .map_err(|_| OAuthError::Store("OAuth credential serialization failed".into()))?,
        );
        self.write_document(&document)?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn fail_refresh_persistence(&self, fail: bool) {
        self.refresh_write_failure.store(fail, Ordering::Release);
    }

    pub fn remove(&self, provider: OAuthProvider) -> Result<(), OAuthError> {
        let _store_guard = self.lock_exclusive()?;
        self.remove_locked(provider)
    }

    pub(crate) fn remove_locked(&self, provider: OAuthProvider) -> Result<(), OAuthError> {
        let _guard = STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| OAuthError::Store("auth store lock poisoned".into()))?;
        let mut document = self.read_document()?;
        let had_oauth = document
            .pointer(&format!("/providers/{}/oauth", provider.key()))
            .is_some();
        if had_oauth && preferred_method(&document, provider)?.is_none() {
            document
                .pointer_mut(&format!("/providers/{}", provider.key()))
                .and_then(Value::as_object_mut)
                .ok_or_else(|| OAuthError::Store("provider auth entry must be an object".into()))?
                .insert("preferred_method".into(), Value::String("oauth".into()));
        }
        if let Some(providers) = document.get_mut("providers").and_then(Value::as_object_mut) {
            let remove_entry = providers
                .get_mut(provider.key())
                .and_then(Value::as_object_mut)
                .is_some_and(|entry| {
                    entry.remove("oauth");
                    entry.is_empty()
                });
            if remove_entry {
                providers.remove(provider.key());
            }
        }
        if document.get("active_provider").and_then(Value::as_str) == Some(provider.key()) {
            let entry = document.pointer(&format!("/providers/{}", provider.key()));
            let has_selected_credential = match entry
                .and_then(|entry| entry.get("preferred_method"))
                .and_then(Value::as_str)
            {
                Some("oauth") => entry.and_then(|entry| entry.get("oauth")).is_some(),
                Some("api_key") => entry.and_then(|entry| entry.get("api_key")).is_some(),
                None => {
                    entry.and_then(|entry| entry.get("oauth")).is_some()
                        || entry.and_then(|entry| entry.get("api_key")).is_some()
                }
                Some(_) => false,
            };
            if !has_selected_credential {
                document
                    .as_object_mut()
                    .map(|object| object.remove("active_provider"));
            }
        }
        self.write_document(&document)
    }

    pub(crate) fn lock_exclusive(&self) -> Result<StoreGuard, OAuthError> {
        crate::auth::lock_auth_store(&self.path)
            .map(|lock| StoreGuard { _lock: lock })
            .map_err(|error| match error {
                crate::auth::AuthError::Locked => {
                    OAuthError::Store("auth store lock timed out".into())
                }
                other => OAuthError::Store(other.to_string()),
            })
    }

    pub(crate) fn lock_exclusive_until(
        &self,
        deadline: std::time::Instant,
        cancelled: impl Fn() -> bool,
    ) -> Result<StoreGuard, OAuthError> {
        crate::auth::lock_auth_store_until(&self.path, deadline, cancelled)
            .map(|lock| StoreGuard { _lock: lock })
            .map_err(|error| match error {
                crate::auth::AuthError::Locked => {
                    OAuthError::Store("auth store lock timed out".into())
                }
                other => OAuthError::Store(other.to_string()),
            })
    }

    pub(crate) fn lock_refresh(
        &self,
        provider: OAuthProvider,
        cancelled: impl Fn() -> bool,
    ) -> Result<RefreshGuard, OAuthError> {
        crate::auth::lock_oauth_refresh(&self.path, provider.key(), cancelled)
            .map(|lock| RefreshGuard { _lock: lock })
            .map_err(|error| match error {
                crate::auth::OAuthRefreshLockError::Cancelled => OAuthError::Cancelled,
                crate::auth::OAuthRefreshLockError::Auth(crate::auth::AuthError::Locked) => {
                    OAuthError::Store("OAuth refresh lock timed out".into())
                }
                crate::auth::OAuthRefreshLockError::Auth(error) => {
                    OAuthError::Store(error.to_string())
                }
            })
    }

    fn credential_from(
        &self,
        document: &Value,
        provider: OAuthProvider,
    ) -> Option<Result<OAuthCredential, OAuthError>> {
        document
            .pointer(&format!("/providers/{}/oauth", provider.key()))
            .cloned()
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(|_| OAuthError::Store("OAuth credential schema is invalid".into()))
            })
    }

    fn read_document(&self) -> Result<Value, OAuthError> {
        if !self.path.exists() {
            return Ok(json!({"version": 1, "providers": {}}));
        }
        let bytes =
            secure_auth_file(&self.path).map_err(|error| OAuthError::Store(error.to_string()))?;
        let document: Value = serde_json::from_slice(&bytes)
            .map_err(|_| OAuthError::Store("auth file contains malformed JSON".into()))?;
        if document.get("version").and_then(Value::as_u64) != Some(1)
            || !document.get("providers").is_some_and(Value::is_object)
        {
            return Err(OAuthError::Store("auth file schema is invalid".into()));
        }
        crate::auth::validate_auth_document_value(&document)
            .map_err(|_| OAuthError::Store("auth file schema is invalid".into()))?;
        Ok(document)
    }

    fn write_document(&self, document: &Value) -> Result<(), OAuthError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| OAuthError::Store("auth file has no parent directory".into()))?;
        fs::create_dir_all(parent)
            .map_err(|_| OAuthError::Store("auth directory creation failed".into()))?;
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || metadata.is_dir())
        {
            return Err(OAuthError::Store("auth file path is unsafe".into()));
        }
        let suffix = super::pkce::generate_state()?;
        let temporary = parent.join(format!(".auth-{suffix}.tmp"));
        let bytes = serde_json::to_vec_pretty(document)
            .map_err(|_| OAuthError::Store("auth serialization failed".into()))?;
        let _cleanup = TemporaryFile(temporary.clone());
        let mut file = create_secure_auth_file(&temporary)
            .map_err(|error| OAuthError::Store(error.to_string()))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| OAuthError::Store("auth write failed".into()))?;
        drop(file);
        replace_file(&temporary, &self.path)?;
        Ok(())
    }
}

fn preferred_method(
    document: &Value,
    provider: OAuthProvider,
) -> Result<Option<PreferredAuthMethod>, OAuthError> {
    document
        .pointer(&format!("/providers/{}/preferred_method", provider.key()))
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| OAuthError::Store("provider preferred auth method is invalid".into()))
}

fn validate_api_key_provider(provider: &str) -> Result<(), OAuthError> {
    if matches!(
        provider,
        "opencode-go" | "opencode-zen" | "clinepass" | "command-code" | "xai"
    ) {
        Ok(())
    } else {
        Err(OAuthError::Store("unsupported API-key provider".into()))
    }
}

fn parse_provider(value: &str) -> Option<OAuthProvider> {
    match value {
        "anthropic" => Some(OAuthProvider::Anthropic),
        "openai-codex" => Some(OAuthProvider::OpenAiCodex),
        "xai" => Some(OAuthProvider::Xai),
        _ => None,
    }
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), OAuthError> {
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
        Err(OAuthError::Store("atomic auth replacement failed".into()))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), OAuthError> {
    fs::rename(source, destination)
        .map_err(|_| OAuthError::Store("atomic auth replacement failed".into()))
}
