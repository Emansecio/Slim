use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
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
/// over this file. Slim never writes this file.
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
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthProvider {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oauth: Option<serde_json::Value>,
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
/// Precedence: `SLIM_API_KEY` / provider env → file `oauth` → file `api_key`.
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
    let _ = document.active_provider.as_deref();

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
    if let Some(oauth_value) = provider.oauth {
        let oauth: crate::oauth::OAuthCredential =
            serde_json::from_value(oauth_value).map_err(|_| AuthError::InvalidSchema)?;
        if oauth.access.trim().is_empty() {
            return Err(AuthError::InvalidSchema);
        }
        return Ok(Some(ProviderCredential {
            access: oauth.access,
            account_id: oauth.account_id,
            oauth: true,
        }));
    }
    let Some(key) = provider.api_key else {
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
        set_provider(
            &mut document.providers,
            kind,
            Some(AuthProvider {
                api_key: Some(api_key.to_owned()),
                oauth: None,
            }),
        );
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
        set_provider(&mut document.providers, kind, None);
        if document.active_provider.as_deref() == Some(provider_name(kind)) {
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

fn set_provider(providers: &mut AuthProviders, kind: ProviderKind, provider: Option<AuthProvider>) {
    match kind {
        ProviderKind::OpenAiCompatible => providers.openai_compatible = provider,
        ProviderKind::OpenAiCodex => providers.openai_codex = provider,
        ProviderKind::Anthropic => providers.anthropic = provider,
        ProviderKind::OpenCodeGo => providers.opencode_go = provider,
        ProviderKind::OpenCodeZen => providers.opencode_zen = provider,
        ProviderKind::ClinePass => providers.clinepass = provider,
        ProviderKind::CommandCode => providers.command_code = provider,
        ProviderKind::Xai => providers.xai = provider,
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

pub(crate) fn auth_lock_path(auth_file: &Path) -> Result<PathBuf, AuthError> {
    Ok(auth_file
        .parent()
        .ok_or(AuthError::InvalidPath)?
        .join(".auth.lock"))
}

pub(crate) fn lock_auth_store(auth_file: &Path) -> Result<AuthStoreLock, AuthError> {
    let parent = auth_file.parent().ok_or(AuthError::InvalidPath)?;
    fs::create_dir_all(parent).map_err(|_| AuthError::Write)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let lock_path = auth_lock_path(auth_file)?;
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
                Ok(file) => return Ok(AuthStoreLock { _file: file }),
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Err(_) => return Err(AuthError::Locked),
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = parent;
        Ok(AuthStoreLock {})
    }
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

const SENSITIVE_HEADER_NAMES: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "cookie",
    "set-cookie",
    "x-auth-token",
    "x-amz-security-token",
];

pub fn redact(input: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(input) {
        redact_json_value(&mut value);
        if let Ok(redacted) = serde_json::to_string(&value) {
            return redacted;
        }
    }
    let mut redacted = String::with_capacity(input.len());
    for line in input.split_inclusive('\n') {
        let (content, newline) = if let Some(content) = line.strip_suffix("\r\n") {
            (content, "\r\n")
        } else {
            line.strip_suffix('\n')
                .map_or((line, ""), |content| (content, "\n"))
        };
        redacted.push_str(&redact_header_line(content));
        redacted.push_str(newline);
    }
    redacted
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (name, value) in object {
                if SENSITIVE_HEADER_NAMES
                    .iter()
                    .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
                {
                    *value = serde_json::Value::String("[REDACTED]".into());
                } else {
                    redact_json_value(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value);
            }
        }
        _ => {}
    }
}

fn redact_header_line(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut search_from = 0;
    let mut matches = Vec::new();
    while search_from < bytes.len() {
        let Some((index, header_len, value_start)) =
            find_sensitive_header(line, search_from, SENSITIVE_HEADER_NAMES)
        else {
            break;
        };
        matches.push((index, value_start));
        search_from = index + header_len;
    }
    if matches.is_empty() {
        return line.to_owned();
    }

    let mut result = String::with_capacity(line.len() + matches.len() * 4);
    let mut cursor = 0;
    for (index, (_, value_start)) in matches.iter().enumerate() {
        let end = matches
            .get(index + 1)
            .map_or(line.len(), |(next_index, _)| *next_index);
        result.push_str(&line[cursor..*value_start]);
        result.push_str("[REDACTED]");
        cursor = end;
    }
    result.push_str(&line[cursor..]);
    result
}

fn find_sensitive_header(
    line: &str,
    search_from: usize,
    header_names: &[&str],
) -> Option<(usize, usize, usize)> {
    let bytes = line.as_bytes();
    let mut best = None;
    for name in header_names {
        let Some(offset) = find_ascii_case_insensitive(&line[search_from..], name) else {
            continue;
        };
        let index = search_from + offset;
        let boundary_before =
            index == 0 || !bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b'-';
        if !boundary_before {
            continue;
        }
        let mut cursor = index + name.len();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b':' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if best.is_none_or(|(best_index, _, _)| index < best_index) {
            best = Some((index, name.len(), cursor));
        }
    }
    best
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
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
