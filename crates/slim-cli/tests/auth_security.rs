use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use slim_cli::{
    delete_api_key_file, load_auth_credential, load_auth_file, redact, resolve_api_key,
    resolve_provider_credential, save_api_key_file, AuthError,
};
use slim_core::provider::ProviderKind;
#[cfg(windows)]
use std::mem::size_of;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::ptr::{null, null_mut};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, GENERIC_READ, HANDLE,
    INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
#[cfg(windows)]
use windows_sys::Win32::Security::{
    AclSizeInformation, CreateWellKnownSid, EqualSid, GetAce, GetAclInformation, GetLengthSid,
    GetSecurityDescriptorControl, GetTokenInformation, IsValidSid, TokenUser,
    WinBuiltinAdministratorsSid, WinLocalSystemSid, WinWorldSid, ACCESS_ALLOWED_ACE, ACE_HEADER,
    ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, SECURITY_MAX_SID_SIZE, SE_DACL_PRESENT,
    SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ALL_ACCESS,
    FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, OPEN_EXISTING,
    READ_CONTROL,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "slim-auth-{label}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&path).expect("create temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    #[cfg(windows)]
    fn deterministic(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("slim-auth-{label}-{}", std::process::id()));
        fs::create_dir_all(&path).expect("create deterministic temp directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}

fn auth_file(temp: &TempDir, contents: &str) -> PathBuf {
    let path = temp.path().join("auth.json");
    fs::write(&path, contents).expect("write auth fixture");
    path
}

#[cfg(windows)]
fn assert_native_acl_is_narrow(path: &Path) {
    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_READ | READ_CONTROL,
            FILE_SHARE_READ,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    assert_ne!(handle, INVALID_HANDLE_VALUE);
    assert!(!handle.is_null());

    let mut attributes = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    assert_ne!(
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileAttributeTagInfo,
                &mut attributes as *mut _ as *mut _,
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        },
        0
    );
    assert_eq!(attributes.ReparseTag, 0);

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
    assert_eq!(status, 0);
    assert!(!dacl.is_null());
    let mut control = 0u16;
    let mut revision = 0u32;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
        0
    );
    assert_eq!(
        control & (SE_DACL_PRESENT | SE_DACL_PROTECTED),
        SE_DACL_PRESENT | SE_DACL_PROTECTED
    );

    let mut size_info = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    assert_ne!(
        unsafe {
            GetAclInformation(
                dacl,
                &mut size_info as *mut _ as *mut _,
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        },
        0
    );
    assert_eq!(size_info.AceCount, 3);

    let current_sid = current_user_sid_for_test();
    let system_sid = well_known_sid_for_test(WinLocalSystemSid);
    let administrators_sid = well_known_sid_for_test(WinBuiltinAdministratorsSid);
    for index in 0..size_info.AceCount {
        let mut ace = null_mut();
        assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        assert_eq!(header.AceType, 0);
        assert_eq!(header.AceFlags, 0);
        assert!(header.AceSize as usize >= size_of::<ACCESS_ALLOWED_ACE>());
        let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        assert_eq!(allowed.Mask, FILE_ALL_ACCESS);
        let sid = &allowed.SidStart as *const u32 as *mut _;
        assert_ne!(unsafe { IsValidSid(sid) }, 0);
        assert!(
            unsafe { EqualSid(sid, current_sid.as_ptr() as *mut _) != 0 }
                || unsafe { EqualSid(sid, system_sid.as_ptr() as *mut _) != 0 }
                || unsafe { EqualSid(sid, administrators_sid.as_ptr() as *mut _) != 0 }
        );
        assert_eq!(
            unsafe { windows_sys::Win32::Security::IsWellKnownSid(sid, WinWorldSid) },
            0
        );
    }

    unsafe {
        let _ = CloseHandle(handle);
        let _ = windows_sys::Win32::Foundation::LocalFree(descriptor);
    }
}

#[cfg(windows)]
fn current_user_sid_for_test() -> Vec<u8> {
    let mut token: HANDLE = null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let mut required = 0u32;
    assert_eq!(
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut required) },
        0
    );
    assert_eq!(unsafe { GetLastError() }, ERROR_INSUFFICIENT_BUFFER);
    let mut buffer = vec![0u8; required as usize];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut _,
                required,
                &mut required,
            )
        },
        0
    );
    let token_user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    let length = unsafe { GetLengthSid(token_user.User.Sid) } as usize;
    unsafe {
        let _ = CloseHandle(token);
        std::slice::from_raw_parts(token_user.User.Sid as *const u8, length).to_vec()
    }
}

#[cfg(windows)]
fn well_known_sid_for_test(kind: windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE) -> Vec<u8> {
    let mut buffer = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = buffer.len() as u32;
    assert_ne!(
        unsafe { CreateWellKnownSid(kind, null_mut(), buffer.as_mut_ptr() as *mut _, &mut size) },
        0
    );
    buffer.truncate(size as usize);
    buffer
}

struct EnvGuard {
    values: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn capture(names: &[&'static str]) -> Self {
        Self {
            values: names
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.values {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
    }
}

#[test]
fn environment_keys_have_priority_over_auth_file() {
    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock");
    let _env = EnvGuard::capture(&["SLIM_API_KEY", "OPENAI_API_KEY", "SLIM_AUTH_FILE"]);
    let temp = TempDir::new("env-priority");
    let path = auth_file(
        &temp,
        r#"{"version": "wrong", "providers": {"openai-compatible": {"api_key": "fixture-file"}}}"#,
    );
    std::env::set_var("SLIM_AUTH_FILE", &path);
    std::env::set_var("OPENAI_API_KEY", "fixture-provider");
    std::env::set_var("SLIM_API_KEY", "fixture-global");
    let key = resolve_api_key(ProviderKind::OpenAiCompatible).expect("global env key");
    assert!(matches!(key.as_deref(), Some("fixture-global")));

    std::env::remove_var("SLIM_API_KEY");
    let key = resolve_api_key(ProviderKind::OpenAiCompatible).expect("provider env key");
    assert!(matches!(key.as_deref(), Some("fixture-provider")));
}

#[test]
fn opencode_key_precedence_is_global_then_provider_then_file() {
    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock");
    let _env = EnvGuard::capture(&["SLIM_API_KEY", "OPENCODE_API_KEY", "SLIM_AUTH_FILE"]);
    let temp = TempDir::new("opencode-priority");
    let path = auth_file(
        &temp,
        r#"{"version":1,"providers":{"opencode-go":{"api_key":"file-key"}}}"#,
    );
    std::env::set_var("SLIM_AUTH_FILE", path);
    std::env::set_var("OPENCODE_API_KEY", "provider-key");
    std::env::set_var("SLIM_API_KEY", "global-key");
    assert_eq!(
        resolve_api_key(ProviderKind::OpenCodeGo)
            .expect("global key")
            .as_deref(),
        Some("global-key")
    );
    std::env::remove_var("SLIM_API_KEY");
    assert_eq!(
        resolve_api_key(ProviderKind::OpenCodeGo)
            .expect("provider key")
            .as_deref(),
        Some("provider-key")
    );
    std::env::remove_var("OPENCODE_API_KEY");
    assert_eq!(
        resolve_api_key(ProviderKind::OpenCodeGo)
            .expect("file key")
            .as_deref(),
        Some("file-key")
    );
}

#[test]
fn auth_file_supports_both_providers() {
    let temp = TempDir::new("providers");
    let path = auth_file(
        &temp,
        r#"{"version":1,"providers":{"openai-compatible":{"api_key":"fixture-openai"},"anthropic":{"api_key":"fixture-anthropic"}}}"#,
    );
    let openai = load_auth_file(&path, ProviderKind::OpenAiCompatible).expect("openai key");
    let anthropic = load_auth_file(&path, ProviderKind::Anthropic).expect("anthropic key");
    assert!(matches!(openai.as_deref(), Some("fixture-openai")));
    assert!(matches!(anthropic.as_deref(), Some("fixture-anthropic")));
}

#[test]
fn auth_file_oauth_without_api_key_yields_access_token() {
    let temp = TempDir::new("oauth-only");
    let path = auth_file(
        &temp,
        r#"{"version":1,"providers":{"openai-codex":{"oauth":{"access":"oauth-access","refresh":"oauth-refresh","expires":1,"account_id":"acct-1"}}}}"#,
    );
    assert_eq!(
        load_auth_file(&path, ProviderKind::OpenAiCodex)
            .expect("oauth access")
            .as_deref(),
        Some("oauth-access")
    );
    let credential = load_auth_credential(&path, ProviderKind::OpenAiCodex)
        .expect("oauth credential")
        .expect("present");
    assert_eq!(credential.access, "oauth-access");
    assert_eq!(credential.account_id.as_deref(), Some("acct-1"));
    assert!(credential.oauth);
}

#[test]
fn auth_file_oauth_wins_over_leftover_api_key() {
    let temp = TempDir::new("oauth-over-key");
    let path = auth_file(
        &temp,
        r#"{"version":1,"providers":{"openai-codex":{"api_key":"stale-key","oauth":{"access":"oauth-access","refresh":"oauth-refresh","expires":1,"account_id":"acct-1"}}}}"#,
    );
    let credential = load_auth_credential(&path, ProviderKind::OpenAiCodex)
        .expect("credential")
        .expect("present");
    assert_eq!(credential.access, "oauth-access");
    assert_eq!(credential.account_id.as_deref(), Some("acct-1"));
    assert!(credential.oauth);
}

#[test]
fn leftover_auth_lock_file_does_not_block_save() {
    let temp = TempDir::new("stale-lock");
    let path = auth_file(
        &temp,
        r#"{"version":1,"providers":{"opencode-go":{"api_key":"old"}}}"#,
    );
    fs::write(temp.path().join(".auth.lock"), b"stale").expect("stale lock");
    save_api_key_file(&path, ProviderKind::OpenCodeGo, "fresh-key").expect("save over stale lock");
    assert_eq!(
        load_auth_file(&path, ProviderKind::OpenCodeGo)
            .expect("fresh")
            .as_deref(),
        Some("fresh-key")
    );
}

#[test]
fn environment_api_key_wins_over_oauth_file() {
    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock");
    let _env = EnvGuard::capture(&["SLIM_API_KEY", "CODEX_ACCESS_TOKEN", "SLIM_AUTH_FILE"]);
    let temp = TempDir::new("oauth-env-wins");
    let path = auth_file(
        &temp,
        r#"{"version":1,"providers":{"openai-codex":{"oauth":{"access":"oauth-access","refresh":"oauth-refresh","expires":1,"account_id":"acct-1"}}}}"#,
    );
    std::env::set_var("SLIM_AUTH_FILE", &path);
    std::env::set_var("SLIM_API_KEY", "env-wins");
    std::env::remove_var("CODEX_ACCESS_TOKEN");
    let credential = resolve_provider_credential(ProviderKind::OpenAiCodex)
        .expect("env credential")
        .expect("present");
    assert_eq!(credential.access, "env-wins");
    assert!(!credential.oauth);
}

#[test]
fn opencode_key_save_is_atomic_and_preserves_sibling_provider() {
    let temp = TempDir::new("save-opencode");
    let path = auth_file(
        &temp,
        r#"{"version":1,"providers":{"openai-compatible":{"api_key":"fixture-openai"},"anthropic":null}}"#,
    );

    save_api_key_file(&path, ProviderKind::OpenCodeGo, "fixture-opencode")
        .expect("save OpenCode key");

    assert_eq!(
        load_auth_file(&path, ProviderKind::OpenCodeGo)
            .expect("OpenCode key")
            .as_deref(),
        Some("fixture-opencode")
    );
    assert_eq!(
        load_auth_file(&path, ProviderKind::OpenAiCompatible)
            .expect("sibling key")
            .as_deref(),
        Some("fixture-openai")
    );
    #[cfg(windows)]
    assert_native_acl_is_narrow(&path);
}

#[test]
fn opencode_logout_deletes_only_its_persisted_key() {
    let temp = TempDir::new("delete-opencode");
    let path = auth_file(
        &temp,
        r#"{"version":1,"active_provider":"opencode-go","providers":{"openai-compatible":{"api_key":"fixture-openai"},"opencode-go":{"api_key":"fixture-opencode"}}}"#,
    );

    delete_api_key_file(&path, ProviderKind::OpenCodeGo).expect("delete OpenCode key");

    assert!(load_auth_file(&path, ProviderKind::OpenCodeGo)
        .expect("OpenCode key")
        .is_none());
    assert_eq!(
        load_auth_file(&path, ProviderKind::OpenAiCompatible)
            .expect("sibling key")
            .as_deref(),
        Some("fixture-openai")
    );
}

#[test]
fn command_code_key_save_is_atomic_and_preserves_sibling_provider() {
    let temp = TempDir::new("save-command-code");
    let path = auth_file(
        &temp,
        r#"{"version":1,"providers":{"openai-compatible":{"api_key":"fixture-openai"},"anthropic":null}}"#,
    );

    save_api_key_file(&path, ProviderKind::CommandCode, "fixture-cmd")
        .expect("save Command Code key");

    assert_eq!(
        load_auth_file(&path, ProviderKind::CommandCode)
            .expect("Command Code key")
            .as_deref(),
        Some("fixture-cmd")
    );
    assert_eq!(
        load_auth_file(&path, ProviderKind::OpenAiCompatible)
            .expect("sibling key")
            .as_deref(),
        Some("fixture-openai")
    );
}

#[test]
fn api_key_save_rejects_directory_destination_without_deleting_it() {
    let temp = TempDir::new("save-directory");

    let error = save_api_key_file(temp.path(), ProviderKind::OpenCodeGo, "fixture-opencode")
        .expect_err("directory destination must fail closed");

    assert_eq!(error, AuthError::InvalidPath);
    assert!(temp.path().is_dir());
}

#[test]
fn default_user_profile_path_is_used_when_explicit_path_is_absent() {
    let _lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock");
    let _env = EnvGuard::capture(&[
        "SLIM_AUTH_FILE",
        "USERPROFILE",
        "SLIM_API_KEY",
        "OPENAI_API_KEY",
    ]);
    let temp = TempDir::new("default-path");
    let slim_dir = temp.path().join(".slim");
    fs::create_dir_all(&slim_dir).expect("create slim directory");
    fs::write(
        slim_dir.join("auth.json"),
        r#"{"version":1,"providers":{"openai-compatible":{"api_key":"fixture-default"}}}"#,
    )
    .expect("write default auth fixture");
    std::env::remove_var("SLIM_AUTH_FILE");
    std::env::set_var("USERPROFILE", temp.path());
    std::env::remove_var("SLIM_API_KEY");
    std::env::remove_var("OPENAI_API_KEY");

    let key = resolve_api_key(ProviderKind::OpenAiCompatible).expect("default auth key");
    assert!(matches!(key.as_deref(), Some("fixture-default")));
}

#[test]
fn missing_auth_file_is_normal() {
    let temp = TempDir::new("missing");
    let path = temp.path().join("missing.json");
    let key = load_auth_file(&path, ProviderKind::Anthropic).expect("missing auth file");
    assert!(key.is_none());
}

#[test]
fn oversized_auth_file_is_rejected_before_allocation() {
    let temp = TempDir::new("oversized");
    let path = temp.path().join("auth.json");
    fs::File::create(&path)
        .expect("create")
        .set_len(1024 * 1024 + 1)
        .expect("extend");

    let error = load_auth_file(&path, ProviderKind::OpenAiCompatible)
        .expect_err("oversized auth file must fail");

    assert_eq!(error, AuthError::Read);
}

#[test]
fn invalid_json_schema_version_types_and_empty_keys_are_rejected() {
    let cases = [
        ("malformed", "{not-json", AuthError::MalformedJson),
        (
            "wrong-type",
            r#"{"version":1,"providers":{"openai-compatible":{"api_key":42}}}"#,
            AuthError::InvalidSchema,
        ),
        (
            "unknown-version",
            r#"{"version":2,"providers":{"openai-compatible":{"api_key":"fixture"}}}"#,
            AuthError::UnsupportedVersion,
        ),
        (
            "empty-key",
            r#"{"version":1,"providers":{"openai-compatible":{"api_key":"  "}}}"#,
            AuthError::InvalidSchema,
        ),
    ];
    for (label, contents, expected) in cases {
        let temp = TempDir::new(label);
        let path = auth_file(&temp, contents);
        let error = load_auth_file(&path, ProviderKind::OpenAiCompatible)
            .expect_err("invalid auth fixture must fail");
        assert_eq!(error, expected);
        assert!(!error.to_string().contains("fixture"));
    }
}

#[test]
fn directory_path_fails_closed_without_changing_the_directory() {
    let temp = TempDir::new("unsafe-path");
    let error = load_auth_file(temp.path(), ProviderKind::OpenAiCompatible)
        .expect_err("directory is not an auth file");
    assert_eq!(error, AuthError::InvalidPath);
    assert!(temp.path().is_dir());
}

#[test]
fn auth_errors_and_redaction_do_not_include_fixture_secrets() {
    let temp = TempDir::new("redaction");
    let path = auth_file(
        &temp,
        r#"{"version":1,"providers":{"openai-compatible":{"api_key":"fixture-secret"}}}"#,
    );
    let error = load_auth_file(&path, ProviderKind::Anthropic).expect("unconfigured provider");
    assert!(error.is_none());
    let redacted = redact("Authorization: Bearer fixture-secret");
    assert!(!redacted.contains("fixture-secret"));
}

#[test]
fn redaction_masks_every_sensitive_header_in_multiline_input() {
    let input = concat!(
        "Authorization: Bearer placeholder-one\r\n",
        "x-API-KEY: placeholder-two Authorization: Bearer placeholder-four\n",
        "AUTHORIZATION: Basic placeholder-three"
    );
    let redacted = redact(input);
    assert!(!redacted.contains("placeholder-one"));
    assert!(!redacted.contains("placeholder-two"));
    assert!(!redacted.contains("placeholder-three"));
    assert!(!redacted.contains("placeholder-four"));
    assert_eq!(redacted.matches("[REDACTED]").count(), 4);
}

#[test]
fn redaction_masks_sensitive_headers_and_nested_json_values() {
    let input = serde_json::json!({
        "authorization": "Bearer json-one",
        "nested": {
            "api-key": "json-two",
            "x-goog-api-key": "json-three",
            "cookie": "json-four",
            "set-cookie": "json-five",
            "proxy-authorization": "json-six",
            "x-auth-token": "json-seven",
            "x-amz-security-token": "json-eight"
        },
        "safe": "visible"
    })
    .to_string();
    let redacted = redact(&input);
    for secret in [
        "json-one",
        "json-two",
        "json-three",
        "json-four",
        "json-five",
        "json-six",
        "json-seven",
        "json-eight",
    ] {
        assert!(!redacted.contains(secret), "leaked {secret}");
    }
    assert!(redacted.contains("visible"));

    let headers = concat!(
        "api-key: header-one\n",
        "x-goog-api-key: header-two\n",
        "Cookie: header-three\n",
        "Set-Cookie: header-four\n",
        "Proxy-Authorization: header-five\n",
        "X-Auth-Token: header-six\n",
        "X-Amz-Security-Token: header-seven"
    );
    let redacted = redact(headers);
    for secret in [
        "header-one",
        "header-two",
        "header-three",
        "header-four",
        "header-five",
        "header-six",
        "header-seven",
    ] {
        assert!(!redacted.contains(secret), "leaked {secret}");
    }
}

#[cfg(windows)]
#[test]
fn native_acl_handles_accented_path_and_removes_extra_access() {
    let temp = TempDir::deterministic("áccênt-directory");
    let path = auth_file(
        &temp,
        r#"{"version":1,"providers":{"openai-compatible":{"api_key":"placeholder-unicode"}}}"#,
    );
    let key = load_auth_file(&path, ProviderKind::OpenAiCompatible)
        .expect("Unicode auth file")
        .expect("configured Unicode auth key");
    assert_eq!(key, "placeholder-unicode");
    assert_native_acl_is_narrow(&path);
}

#[test]
fn auth_error_remains_debuggable() {
    fn requires_debug<T: std::fmt::Debug>() {}

    requires_debug::<AuthError>();
}
