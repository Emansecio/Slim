use std::fs;
use std::path::{Path, PathBuf};

use super::ToolError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilePrecondition {
    ExactText(String),
}

pub fn write_file(
    path: impl AsRef<Path>,
    content: &str,
    precondition: Option<FilePrecondition>,
) -> Result<(), ToolError> {
    let path = path.as_ref();
    let exists = path.exists();
    if exists && precondition.is_none() {
        return Err(ToolError::PreconditionRequired {
            path: path.display().to_string(),
        });
    }
    if let Some(FilePrecondition::ExactText(expected)) = precondition {
        let actual = fs::read_to_string(path)?;
        if actual != expected {
            return Err(ToolError::StaleRead {
                path: path.display().to_string(),
            });
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = temp_path(path);
    fs::write(&temp, content)?;
    replace_file(&temp, path)?;
    Ok(())
}

fn replace_file(temp: &Path, target: &Path) -> Result<(), ToolError> {
    #[cfg(windows)]
    {
        if target.exists() {
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
            return Ok(());
        }
    }

    fs::rename(temp, target)?;
    Ok(())
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
