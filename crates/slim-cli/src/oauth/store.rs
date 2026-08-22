use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{OAuthCredential, OAuthError, OAuthProvider};
use crate::auth::{auth_file_path, secure_auth_file};

static STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) struct StoreGuard {
    #[cfg(windows)]
    _file: File,
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
}

impl OAuthStore {
    pub fn default_path() -> Result<Self, OAuthError> {
        let path = auth_file_path()
            .map_err(|error| OAuthError::Store(error.to_string()))?
            .ok_or_else(|| OAuthError::Store("USERPROFILE is unavailable".into()))?;
        Ok(Self { path })
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
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
        self.credential_from(&document, provider)
            .transpose()
            .map(|credential| credential.map(|credential| (provider, credential)))
    }

    pub fn credential(
        &self,
        provider: OAuthProvider,
    ) -> Result<Option<OAuthCredential>, OAuthError> {
        self.credential_from(&self.read_document()?, provider)
            .transpose()
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
        self.write_document(&document)
    }

    pub fn remove(&self, provider: OAuthProvider) -> Result<(), OAuthError> {
        let _store_guard = self.lock_exclusive()?;
        let _guard = STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| OAuthError::Store("auth store lock poisoned".into()))?;
        let mut document = self.read_document()?;
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
            document
                .as_object_mut()
                .map(|object| object.remove("active_provider"));
        }
        self.write_document(&document)
    }

    #[cfg(windows)]
    pub(crate) fn lock_exclusive(&self) -> Result<StoreGuard, OAuthError> {
        use std::os::windows::fs::OpenOptionsExt;

        let parent = self
            .path
            .parent()
            .ok_or_else(|| OAuthError::Store("auth file has no parent directory".into()))?;
        fs::create_dir_all(parent)
            .map_err(|_| OAuthError::Store("auth directory creation failed".into()))?;
        let lock_path = parent.join(".auth.lock");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(&lock_path)
            {
                Ok(file) => return Ok(StoreGuard { _file: file }),
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Err(_) => return Err(OAuthError::Store("auth store lock timed out".into())),
            }
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn lock_exclusive(&self) -> Result<StoreGuard, OAuthError> {
        Ok(StoreGuard {})
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
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| OAuthError::Store("auth temporary file creation failed".into()))?;
        let _cleanup = TemporaryFile(temporary.clone());
        drop(file);
        secure_auth_file(&temporary).map_err(|error| OAuthError::Store(error.to_string()))?;
        file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|_| OAuthError::Store("secured auth temporary file open failed".into()))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| OAuthError::Store("auth write failed".into()))?;
        drop(file);
        replace_file(&temporary, &self.path)?;
        Ok(())
    }
}

fn parse_provider(value: &str) -> Option<OAuthProvider> {
    match value {
        "anthropic" => Some(OAuthProvider::Anthropic),
        "openai-codex" => Some(OAuthProvider::OpenAiCodex),
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
