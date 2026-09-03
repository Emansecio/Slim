use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::{
    digest_bytes, DependencyKind, DependencyObservation, FastStamp, ToolError, ToolExecutionError,
};

pub const MAX_MUTATING_FILE_BYTES: usize = 10 * 1024 * 1024;
static MUTATION_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilePrecondition {
    ExactText(String),
}

pub(crate) struct WriteReceipt {
    pub(crate) before: Option<String>,
    pub(crate) dependency: Option<DependencyObservation>,
    pub(crate) after: FastStamp,
    pub(crate) bytes_read: u64,
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
    write_file_with_receipt(path, content, precondition)
        .map(|_| ())
        .map_err(|failure| failure.error)
}

pub(crate) fn write_file_with_receipt(
    path: impl AsRef<Path>,
    content: &str,
    precondition: Option<FilePrecondition>,
) -> Result<WriteReceipt, ToolExecutionError> {
    let _mutation_guard = lock_mutations();
    let path = path.as_ref();
    ensure_mutation_size(content.len())?;
    let exists = path.exists();
    if exists && precondition.is_none() {
        return Err(ToolError::PreconditionRequired {
            path: path.display().to_string(),
        }
        .into());
    }
    let observed = if let Some(FilePrecondition::ExactText(expected)) = precondition {
        let observed = read_existing_file_observed(path)?;
        if observed.content != expected {
            return Err(ToolExecutionError::observed(
                ToolError::StaleRead {
                    path: path.display().to_string(),
                },
                vec![observed.dependency],
                observed.bytes_read,
            ));
        }
        Some(observed)
    } else {
        None
    };
    if let Some(observed) = observed {
        return replace_observed_file(path, observed, content);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let (temp, file, after) = prepare_replacement(path, content)?;
    drop(file);
    if let Err(error) = replace_file(&temp, path, exists) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(WriteReceipt {
        before: None,
        dependency: None,
        after,
        bytes_read: 0,
    })
}

pub(super) fn lock_mutations() -> MutexGuard<'static, ()> {
    MUTATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn replace_observed_file(
    path: &Path,
    observed: ObservedText,
    content: &str,
) -> Result<WriteReceipt, ToolExecutionError> {
    let dependency = observed.dependency.clone();
    let bytes_read = observed.bytes_read;
    let observed_failure = |error: ToolError| {
        ToolExecutionError::observed(error, vec![dependency.clone()], bytes_read)
    };
    let (temp, file, after) =
        prepare_replacement(path, content).map_err(|failure| observed_failure(failure.error))?;
    let current_matches = current_file_matches(&observed._guard, &observed.dependency)
        .map_err(|error| observed_failure(error.into()))?;
    if !current_matches {
        let _ = fs::remove_file(&temp);
        return Err(observed_failure(ToolError::StaleRead {
            path: path.display().to_string(),
        }));
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
    if let Err(error) = replace_file(&temp, path, true) {
        let _ = fs::remove_file(&temp);
        return Err(ToolExecutionError::observed(
            error,
            vec![dependency],
            bytes_read,
        ));
    }
    Ok(WriteReceipt {
        before: Some(before),
        dependency: Some(dependency),
        after,
        bytes_read,
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

pub(super) fn read_existing_file_observed(path: &Path) -> Result<ObservedText, ToolExecutionError> {
    let mut file = open_precondition_file(path)?;
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

fn replace_file(temp: &Path, target: &Path, replace_existing: bool) -> Result<(), ToolError> {
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
        let result = unsafe {
            ReplaceFileW(
                replaced.as_ptr(),
                replacement.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if result == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        fs::rename(temp, target)?;
        Ok(())
    }
}

#[cfg(windows)]
fn open_precondition_file(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    let mut options = OpenOptions::new();
    let file = options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)?;
    super::execution::verify_opened_path(&file, path)?;
    file.lock()?;
    Ok(file)
}

#[cfg(not(windows))]
fn open_precondition_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().read(true).open(path)?;
    super::execution::verify_opened_path(&file, path)?;
    file.lock()?;
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
