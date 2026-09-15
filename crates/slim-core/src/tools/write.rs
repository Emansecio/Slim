use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, TryLockError};
use std::time::Duration;

use crate::runtime::CancellationToken;
use sha2::{Digest, Sha256};

use super::{
    digest_bytes, DependencyKind, DependencyObservation, FastStamp, ToolError, ToolExecutionError,
};

pub const MAX_MUTATING_FILE_BYTES: usize = 10 * 1024 * 1024;
static PATH_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilePrecondition {
    ExactText(String),
    /// SHA-256 of the bytes observed by a complete read, before display formatting.
    ObservedDigest([u8; 32]),
}

pub(crate) struct WriteReceipt {
    pub(crate) recovery_note: Option<String>,
    pub(crate) before: Option<String>,
    pub(crate) dependency: Option<DependencyObservation>,
    pub(crate) after: FastStamp,
    pub(crate) bytes_read: u64,
    pub(crate) written_digest: [u8; 32],
    pub(crate) written_sha256_12: String,
}

pub(super) struct ObservedText {
    pub(super) content: String,
    pub(super) dependency: DependencyObservation,
    pub(super) bytes_read: u64,
    _guard: File,
}

pub fn write_file(
    path: impl AsRef<Path>,
    content: &str,
    precondition: Option<FilePrecondition>,
) -> Result<(), ToolError> {
    let path = resolved_public_path(path.as_ref())?;
    write_file_with_receipt(path, content, precondition, None, &mut || {})
        .map(|_| ())
        .map_err(|failure| failure.error)
}

pub(crate) fn write_file_with_receipt(
    path: impl AsRef<Path>,
    content: &str,
    precondition: Option<FilePrecondition>,
    cancellation: Option<&CancellationToken>,
    on_lock_wait: &mut impl FnMut(),
) -> Result<WriteReceipt, ToolExecutionError> {
    let path = path.as_ref();
    let slot = mutation_lock_slot(path);
    let _mutation_guard = lock_mutations(&slot, cancellation)?;
    ensure_mutation_size(content.len())?;
    let mut content = Cow::Borrowed(content);
    let exists = path.exists();
    if exists && precondition.is_none() {
        let observed = read_existing_file_observed(path, cancellation, on_lock_wait)?;
        let mut failure = ToolExecutionError::observed(
            ToolError::PreconditionRequired {
                path: path.display().to_string(),
            },
            vec![observed.dependency],
            observed.bytes_read,
        );
        failure.context = Some(current_file_recovery_context(&observed.content));
        return Err(failure);
    }
    let observed = if let Some(precondition) = precondition {
        if fs::metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound) {
            return Err(ToolError::InvalidInput {
                message: format!(
                    "{}: file does not exist; expected checks an EXISTING file (even when empty). To create it, retry write with the same path/content and omit expected or set it to null. No file was written.",
                    path.display()
                ),
            }
            .into());
        }
        let observed = read_existing_file_observed(path, cancellation, on_lock_wait)?;
        let uniform_crlf = super::patch::has_only_crlf_newlines(&observed.content);
        let (matches, preserve_crlf) = match precondition {
            FilePrecondition::ExactText(expected) => {
                let normalized = observed.content != expected
                    && uniform_crlf
                    && !expected.contains('\r')
                    && observed.content.replace("\r\n", "\n") == expected;
                (observed.content == expected || normalized, normalized)
            }
            FilePrecondition::ObservedDigest(expected) => (
                <[u8; 32]>::from(Sha256::digest(observed.content.as_bytes())) == expected,
                uniform_crlf,
            ),
        };
        if !matches {
            let mut failure = ToolExecutionError::observed(
                ToolError::StaleRead {
                    path: path.display().to_string(),
                },
                vec![observed.dependency],
                observed.bytes_read,
            );
            failure.context = Some(current_file_recovery_context(&observed.content));
            return Err(failure);
        }
        if preserve_crlf {
            content = Cow::Owned(content.replace("\r\n", "\n").replace('\n', "\r\n"));
        }
        Some(observed)
    } else {
        None
    };
    if let Some(observed) = observed {
        return replace_observed_file(path, observed, &content, cancellation);
    }

    if let Some(parent) = path.parent() {
        check_cancellation(cancellation)?;
        fs::create_dir_all(parent)?;
    }
    let (temp, file, after) = prepare_replacement(path, &content)?;
    drop(file);
    if let Err(error) = check_cancellation(cancellation) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    if let Err(error) = replace_file(&temp, path, exists, None) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(WriteReceipt {
        recovery_note: None,
        before: None,
        dependency: None,
        after,
        bytes_read: 0,
        written_digest: content_sha256(content.as_bytes()),
        written_sha256_12: content_sha256_prefix(content.as_bytes()),
    })
}

pub(super) fn check_cancellation(
    cancellation: Option<&CancellationToken>,
) -> Result<(), ToolError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        Err(ToolError::Cancelled)
    } else {
        Ok(())
    }
}

pub(super) fn mutation_lock_slot(path: &Path) -> Arc<Mutex<()>> {
    let key = super::path_identity(path);
    let mut locks = PATH_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if locks.len() > 64 {
        locks.retain(|_, slot| Arc::strong_count(slot) > 1);
    }
    locks
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

pub(super) fn lock_mutations<'a>(
    slot: &'a Arc<Mutex<()>>,
    cancellation: Option<&CancellationToken>,
) -> Result<MutexGuard<'a, ()>, ToolError> {
    loop {
        check_cancellation(cancellation)?;
        match slot.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

pub(super) fn replace_observed_file(
    path: &Path,
    observed: ObservedText,
    content: &str,
    cancellation: Option<&CancellationToken>,
) -> Result<WriteReceipt, ToolExecutionError> {
    replace_observed_file_with_publisher(
        path,
        observed,
        content,
        cancellation,
        |temp, target, backup| replace_file(temp, target, true, Some(backup)),
    )
}

fn replace_observed_file_with_publisher(
    path: &Path,
    observed: ObservedText,
    content: &str,
    cancellation: Option<&CancellationToken>,
    publish: impl FnOnce(&Path, &Path, &Path) -> io::Result<()>,
) -> Result<WriteReceipt, ToolExecutionError> {
    let dependency = observed.dependency.clone();
    let bytes_read = observed.bytes_read;
    let observed_failure = |error: ToolError| {
        ToolExecutionError::observed(error, vec![dependency.clone()], bytes_read)
    };
    ensure_mutation_size(content.len()).map_err(&observed_failure)?;
    // Windows refuses replacing a readonly target; reject before preparing a
    // replacement so this known pre-commit failure creates no recovery files.
    #[cfg(windows)]
    if observed
        ._guard
        .metadata()
        .map_err(|error| observed_failure(error.into()))?
        .permissions()
        .readonly()
    {
        return Err(observed_failure(
            io::Error::new(io::ErrorKind::PermissionDenied, "target is readonly").into(),
        ));
    }
    let (temp, file, after) =
        prepare_replacement(path, content).map_err(|failure| observed_failure(failure.error))?;
    let current_matches = current_file_matches(&observed._guard, &observed.dependency)
        .map_err(|error| observed_failure(error.into()))?;
    if !current_matches {
        let _ = fs::remove_file(&temp);
        drop(file);
        drop(observed);
        return Err(stale_read_after_stamp_mismatch(path, cancellation));
    }
    let ObservedText {
        content: before,
        dependency,
        bytes_read,
        _guard,
    } = observed;
    _guard
        .unlock()
        .map_err(|error| observed_failure(error.into()))?;
    drop(_guard);
    drop(file);
    if let Err(error) = check_cancellation(cancellation) {
        let _ = fs::remove_file(&temp);
        return Err(observed_failure(error));
    }
    let backup = temp.with_extension("displaced");
    if let Err(error) = publish(&temp, path, &backup) {
        // Failed publication can already have moved the original pathname.
        // Never delete recovery data, retry, or roll back over a third edit.
        let message = format!(
            "publication state unresolved (os_code={:?}): {error}; target={}; replacement={}; displaced={}; existing recovery files were preserved; inspect before retrying",
            error.raw_os_error(), path.display(), temp.display(), backup.display());
        let mut failure =
            ToolExecutionError::observed(ToolError::Io { message }, vec![dependency], bytes_read);
        failure.effects_uncertain = true;
        return Err(failure);
    }
    let (recovery_note, verification_bytes) = cleanup_displaced_if_unchanged(&backup, &before);
    Ok(WriteReceipt {
        recovery_note,
        before: Some(before),
        dependency: Some(dependency),
        after,
        bytes_read: bytes_read.saturating_add(verification_bytes),
        written_digest: content_sha256(content.as_bytes()),
        written_sha256_12: content_sha256_prefix(content.as_bytes()),
    })
}

fn prepare_replacement(
    path: &Path,
    content: &str,
) -> Result<(PathBuf, File, FastStamp), ToolExecutionError> {
    let temp = temp_path(path);
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&temp)?;
    if let Err(error) = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_data())
    {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
    };
    let stamp = FastStamp::from_file(
        DependencyKind::File,
        &file,
        &metadata,
        Some(digest_bytes(b"slim-written-content-v1", content.as_bytes())),
    );
    Ok((temp, file, stamp))
}

fn current_file_matches(file: &File, expected: &DependencyObservation) -> io::Result<bool> {
    let metadata = file.metadata()?;
    let current = FastStamp::from_file(DependencyKind::File, file, &metadata, None);
    Ok(current.comparison_digest().is_some()
        && current.comparison_digest() == expected.stamp.comparison_digest())
}

pub(super) fn resolved_public_path(path: &Path) -> Result<PathBuf, ToolError> {
    if path.exists() {
        return Ok(fs::canonicalize(path)?);
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub(super) fn read_existing_file_observed(
    path: &Path,
    cancellation: Option<&CancellationToken>,
    on_lock_wait: &mut impl FnMut(),
) -> Result<ObservedText, ToolExecutionError> {
    let mut file = open_precondition_file(path)?;
    let mut reported_wait = false;
    loop {
        check_cancellation(cancellation)?;
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if !reported_wait {
                    on_lock_wait();
                    reported_wait = true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    let metadata = file.metadata()?;
    let dependency = DependencyObservation {
        path: path.to_path_buf(),
        stamp: FastStamp::from_file(DependencyKind::File, &file, &metadata, None),
    };
    if metadata.len() > MAX_MUTATING_FILE_BYTES as u64 {
        return Err(ToolExecutionError::observed(
            ToolError::InvalidInput {
                message: format!(
                    "file exceeds the {MAX_MUTATING_FILE_BYTES}-byte mutation safety limit"
                ),
            },
            vec![dependency],
            0,
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    if let Err(error) = file.read_to_end(&mut bytes) {
        return Err(ToolExecutionError::observed(
            error.into(),
            vec![dependency],
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        ));
    }
    let bytes_read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let content = String::from_utf8(bytes).map_err(|error| {
        ToolExecutionError::observed(
            io::Error::new(io::ErrorKind::InvalidData, error).into(),
            vec![dependency.clone()],
            bytes_read,
        )
    })?;
    Ok(ObservedText {
        bytes_read,
        content,
        dependency,
        _guard: file,
    })
}

pub(super) const CURRENT_FILE_RECOVERY_BYTES: usize = 64 * 1024;

pub(super) fn current_file_recovery_context(content: &str) -> String {
    if content.len() <= CURRENT_FILE_RECOVERY_BYTES {
        format!(
            "Current file is below; retry write with expected set to this full text, or patch a unique excerpt. Do not read again.\n{content}"
        )
    } else {
        format!(
            "Current file edges are below; middle omitted. Do not pass these edges as expected. Patch a unique excerpt, or read the complete file and then write. Do not pass offset.\n{}",
            recovery_head_tail(content, 16 * 1024)
        )
    }
}

pub(super) fn patch_file_recovery_context(content: &str) -> String {
    if content.len() <= CURRENT_FILE_RECOVERY_BYTES {
        format!(
            "Current file is below; retry patch with a unique exact excerpt from this text, including whitespace and line endings. Do not read again.\n{content}"
        )
    } else {
        format!(
            "Current file edges are below; middle omitted. Do not pass these edges as expected. Search with context_lines or read the complete file, then patch. Do not pass offset.\n{}",
            recovery_head_tail(content, 16 * 1024)
        )
    }
}

fn stale_read_after_stamp_mismatch(
    path: &Path,
    cancellation: Option<&CancellationToken>,
) -> ToolExecutionError {
    match read_existing_file_observed(path, cancellation, &mut || {}) {
        Ok(observed) => {
            let mut failure = ToolExecutionError::observed(
                ToolError::StaleRead {
                    path: path.display().to_string(),
                },
                vec![observed.dependency],
                observed.bytes_read,
            );
            failure.context = Some(current_file_recovery_context(&observed.content));
            failure
        }
        Err(failure) => failure,
    }
}

fn recovery_head_tail(content: &str, keep: usize) -> String {
    if content.len() <= keep {
        return content.to_owned();
    }
    let mut head_end = keep / 2;
    let tail_len = keep - head_end;
    let mut tail_start = content.len() - tail_len;
    while !content.is_char_boundary(head_end) {
        head_end -= 1;
    }
    while !content.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = content
        .len()
        .saturating_sub(head_end)
        .saturating_sub(content.len() - tail_start);
    format!(
        "{}\n[truncated {omitted} bytes by result limit; model sees first {head_end} and last {} bytes]\n{}",
        &content[..head_end],
        content.len() - tail_start,
        &content[tail_start..]
    )
}

pub(super) fn content_sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(super) fn content_sha256_prefix(bytes: &[u8]) -> String {
    let digest = content_sha256(bytes);
    let mut output = String::with_capacity(12);
    for byte in &digest[..6] {
        use std::fmt::Write;
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

pub(super) fn ensure_mutation_size(bytes: usize) -> Result<(), ToolError> {
    if bytes > MAX_MUTATING_FILE_BYTES {
        return Err(ToolError::InvalidInput {
            message: format!(
                "file exceeds the {MAX_MUTATING_FILE_BYTES}-byte mutation safety limit"
            ),
        });
    }
    Ok(())
}

fn cleanup_displaced_if_unchanged(backup: &Path, before: &str) -> (Option<String>, u64) {
    // Deny concurrent writers while comparing/deleting on Windows. If an
    // external writer still holds the displaced object, preserve it too.
    let mut bytes_read = 0;
    let disposable = (|| -> io::Result<bool> {
        let mut file = open_precondition_file(backup)?;
        let mut bytes = Vec::new();
        let read_result = (&mut file)
            .take(MAX_MUTATING_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes);
        bytes_read = bytes.len() as u64;
        read_result?;
        if bytes != before.as_bytes() {
            return Ok(false);
        }
        fs::remove_file(backup)?;
        Ok(true)
    })();
    if matches!(disposable, Ok(true)) {
        (None, bytes_read)
    } else {
        (Some(format!("displaced version preserved at {}; publication uses best-effort preconditions, not compare-and-swap", backup.display())), bytes_read)
    }
}

fn replace_file(
    temp: &Path,
    target: &Path,
    replace_existing: bool,
    backup: Option<&Path>,
) -> io::Result<()> {
    if !replace_existing {
        fs::hard_link(temp, target)?;
        let _ = fs::remove_file(temp);
        return Ok(());
    }

    #[cfg(windows)]
    {
        use std::ffi::c_void;
        use std::os::windows::ffi::OsStrExt;

        extern "system" {
            fn ReplaceFileW(
                replaced_file: *const u16,
                replacement_file: *const u16,
                backup_file: *const u16,
                flags: u32,
                exclude: *mut c_void,
                reserved: *mut c_void,
            ) -> i32;
        }

        let replaced = target
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let replacement = temp
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let backup = backup.map(|path| {
            path.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        });
        let result = unsafe {
            ReplaceFileW(
                replaced.as_ptr(),
                replacement.as_ptr(),
                backup
                    .as_ref()
                    .map_or(std::ptr::null(), |value| value.as_ptr()),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        if let Some(backup) = backup {
            // Preserve the object actually displaced. Publishing the new path
            // must not overwrite a third writer that fills the resulting gap.
            fs::rename(target, backup)?;
            fs::hard_link(temp, target)?;
            let _ = fs::remove_file(temp);
        } else {
            fs::rename(temp, target)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
fn open_precondition_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)?;
    super::execution::verify_opened_path(&file, path)?;
    Ok(file)
}

#[cfg(not(windows))]
fn open_precondition_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().read(true).open(path)?;
    super::execution::verify_opened_path(&file, path)?;
    Ok(file)
}

fn temp_path(path: &Path) -> PathBuf {
    let suffix = format!(
        ".slim-{}.tmp",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    );
    path.with_file_name(format!(
        "{}{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        suffix
    ))
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    fn publication_workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "slim-publication-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::canonicalize(root).unwrap()
    }

    #[test]
    fn publication_failures_preserve_recoverable_layouts() {
        for code in [1175, 1176, 1177, 5] {
            let root = publication_workspace();
            let path = root.join("data.txt");
            fs::write(&path, "before").unwrap();
            let observed = read_existing_file_observed(&path, None, &mut || {}).unwrap();
            let mut recovery = None;
            let failure = replace_observed_file_with_publisher(
                &path,
                observed,
                "after",
                None,
                |temp, target, backup| {
                    recovery = Some((temp.to_owned(), backup.to_owned()));
                    match code {
                        // Include the original no-backup 1176 layout to prove
                        // the only surviving replacement is never cleaned up.
                        1176 => fs::remove_file(target).unwrap(),
                        1177 => fs::rename(target, backup).unwrap(),
                        _ => {}
                    }
                    Err(io::Error::from_raw_os_error(code))
                },
            )
            .err()
            .expect("publication failed");
            let (temp, backup) = recovery.unwrap();
            assert_eq!(fs::read_to_string(&temp).unwrap(), "after");
            if code == 1177 {
                assert_eq!(fs::read_to_string(&backup).unwrap(), "before");
            }
            if code == 1175 || code == 5 {
                assert_eq!(fs::read_to_string(&path).unwrap(), "before");
            }
            assert!(failure.effects_uncertain);
            assert!(failure.mutations.is_empty());
            assert!(format!("{:?}", failure.error).contains("publication state unresolved"));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_publication_preserves_external_edit_after_guard_release() {
        for rename in [false, true] {
            let root = publication_workspace();
            let path = root.join("data.txt");
            fs::write(&path, "before").unwrap();
            let observed = read_existing_file_observed(&path, None, &mut || {}).unwrap();
            let mut displaced = None;
            let written = replace_observed_file_with_publisher(
                &path,
                observed,
                "agent",
                None,
                |temp, target, backup| {
                    if rename {
                        let external = root.join("external.txt");
                        fs::write(&external, "external edit").unwrap();
                        fs::rename(&external, target).unwrap();
                    } else {
                        fs::write(target, "external edit").unwrap();
                    }
                    displaced = Some(backup.to_owned());
                    replace_file(temp, target, true, Some(backup))
                },
            )
            .unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "agent");
            assert_eq!(
                fs::read_to_string(displaced.unwrap()).unwrap(),
                "external edit"
            );
            assert!(written
                .recovery_note
                .unwrap()
                .contains("displaced version preserved"));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn mutation_locks_are_per_path_identity() {
        let a = mutation_lock_slot(Path::new("C:/ws/a.rs"));
        let b = mutation_lock_slot(Path::new("C:/ws/b.rs"));
        let a_slash = mutation_lock_slot(Path::new(r"C:\ws\a.rs"));
        assert!(Arc::ptr_eq(&a, &a_slash));
        assert!(!Arc::ptr_eq(&a, &b));
    }
}
