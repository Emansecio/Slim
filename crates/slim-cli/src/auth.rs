use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use slim_core::provider::ProviderKind;

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
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for AuthError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthDocument {
    version: u32,
    #[serde(default)]
    active_provider: Option<String>,
    providers: AuthProviders,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthProviders {
    #[serde(
        rename = "openai-compatible",
        alias = "openai_compatible",
        alias = "openai"
    )]
    openai_compatible: Option<AuthProvider>,
    #[serde(rename = "openai-codex", default)]
    openai_codex: Option<AuthProvider>,
    anthropic: Option<AuthProvider>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthProvider {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
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

/// Resolve a provider key using environment variables first, then auth.json.
pub fn resolve_api_key(kind: ProviderKind) -> Result<Option<String>, AuthError> {
    let provider_variable = match kind {
        ProviderKind::OpenAiCompatible => "OPENAI_API_KEY",
        ProviderKind::OpenAiCodex => "CODEX_ACCESS_TOKEN",
        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
    };
    if let Some(value) = non_empty_environment_value("SLIM_API_KEY") {
        return Ok(Some(value));
    }
    if let Some(value) = non_empty_environment_value(provider_variable) {
        return Ok(Some(value));
    }

    let Some(path) = auth_file_path()? else {
        return Ok(None);
    };
    load_auth_file(&path, kind)
}

/// Load one provider key from an existing auth file. This is public so the
/// offline contract tests can exercise file loading without making requests.
pub fn load_auth_file(path: &Path, kind: ProviderKind) -> Result<Option<String>, AuthError> {
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
    };
    let Some(provider) = provider else {
        return Ok(None);
    };
    let Some(key) = provider.api_key else {
        let _ = provider.oauth;
        return Ok(None);
    };
    if key.trim().is_empty() {
        return Err(AuthError::InvalidSchema);
    }
    Ok(Some(key))
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

pub fn redact(input: &str) -> String {
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

fn redact_header_line(line: &str) -> String {
    let header_names = ["authorization", "x-api-key"];
    let bytes = line.as_bytes();
    let mut search_from = 0;
    let mut matches = Vec::new();
    while search_from < bytes.len() {
        let Some((index, header_len, value_start)) =
            find_sensitive_header(line, search_from, &header_names)
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

#[cfg(windows)]
mod native_acl {
    use super::AuthError;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER, GENERIC_READ, HANDLE,
        INVALID_HANDLE_VALUE,
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
        GetFileType, ReadFile, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
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
        if unsafe { GetFileSizeEx(handle, &mut size) } == 0 || size < 0 {
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
