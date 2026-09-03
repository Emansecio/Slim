#[cfg(windows)]
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};

use crate::events::SessionEvent;

use super::{SessionHeader, SessionLine};

pub struct SessionWriter {
    path: PathBuf,
    file: File,
    _lock: File,
    last_seq: Option<u64>,
}

#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x00000001;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x00000400;
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x00200000;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    #[link_name = "GetFinalPathNameByHandleW"]
    fn get_final_path_name_by_handle_w(
        file: RawHandle,
        path: *mut u16,
        path_capacity: u32,
        flags: u32,
    ) -> u32;
}

impl SessionWriter {
    pub fn create(path: impl AsRef<Path>, id: &str, cwd: &str) -> io::Result<Self> {
        Self::create_with_header(path, SessionHeader::new(id, cwd, None, None))
    }

    pub(crate) fn create_with_header(
        path: impl AsRef<Path>,
        header: SessionHeader,
    ) -> io::Result<Self> {
        let requested_path = path.as_ref();
        if let Some(parent) = requested_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let path = resolve_session_path(requested_path)?;
        let lock = acquire_lock(&path)?;
        let is_new = !path.exists() || fs::metadata(&path)?.len() == 0;
        let last_seq = if is_new {
            None
        } else {
            let parsed = super::recovery::parse(&path)?;
            if let Some(tail) = parsed.torn_tail.as_ref() {
                super::recovery::quarantine_torn_tail(&path, tail)?;
                let mut options = OpenOptions::new();
                options.write(true);
                let repair = open_checked(options, &path)?;
                repair.set_len(parsed.valid_offset as u64)?;
                repair.sync_data()?;
            } else if parsed.needs_separator {
                let mut options = OpenOptions::new();
                options.append(true);
                let mut repair = open_checked(options, &path)?;
                repair.write_all(b"\n")?;
                repair.sync_data()?;
            }
            parsed.events.last().map(|event| event.seq)
        };
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        let mut file = open_checked(options, &path)?;
        if is_new {
            write_line(&mut file, &SessionLine::from(header))?;
            file.sync_data()?;
        }
        Ok(Self {
            path,
            file,
            _lock: lock,
            last_seq,
        })
    }

    pub fn append(&mut self, event: &SessionEvent) -> io::Result<()> {
        self.append_batch(std::slice::from_ref(event))
    }

    pub fn append_batch(&mut self, events: &[SessionEvent]) -> io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut previous = self.last_seq;
        for event in events {
            if previous.is_some_and(|last_seq| event.seq <= last_seq) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "session event sequence must increase",
                ));
            }
            previous = Some(event.seq);
        }
        let mut bytes = Vec::new();
        for event in events {
            write_line(
                &mut bytes,
                &SessionLine::Event {
                    seq: event.seq,
                    event: event.clone(),
                },
            )?;
        }
        self.file.write_all(&bytes)?;
        self.file.sync_data()?;
        self.last_seq = previous;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn write_line(file: &mut impl Write, line: &SessionLine) -> io::Result<()> {
    let bytes = serde_json::to_vec(line).map_err(io::Error::other)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    Ok(())
}

pub(crate) fn acquire_lock(path: &Path) -> io::Result<File> {
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    #[cfg(windows)]
    options.share_mode(FILE_SHARE_READ);
    open_checked(options, &lock_path)
}

pub(crate) fn open_data_handle(options: OpenOptions, path: &Path) -> io::Result<File> {
    let file = options.open(path)?;
    if is_reparse_metadata(&file.metadata()?) {
        return Err(reparse_error(path));
    }
    Ok(file)
}

#[cfg(windows)]
pub(crate) fn data_path_from_handle(_file: &File, _requested_path: &Path) -> io::Result<PathBuf> {
    let mut capacity = 256usize;
    loop {
        let capacity_u32 = u32::try_from(capacity)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path is too long"))?;
        let mut buffer = vec![0u16; capacity];
        let length = unsafe {
            get_final_path_name_by_handle_w(
                _file.as_raw_handle(),
                buffer.as_mut_ptr(),
                capacity_u32,
                0,
            )
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < capacity {
            buffer.truncate(length);
            return Ok(OsString::from_wide(&buffer).into());
        }
        capacity = length
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path is too long"))?;
    }
}

#[cfg(unix)]
pub(crate) fn data_path_from_handle(file: &File, requested_path: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let path = resolve_session_path(requested_path)?;
    let path_metadata = fs::metadata(&path)?;
    let file_metadata = file.metadata()?;
    if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session path changed while opening",
        ));
    }
    Ok(path)
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn data_path_from_handle(_file: &File, requested_path: &Path) -> io::Result<PathBuf> {
    resolve_session_path(requested_path)
}

pub(crate) fn configure_data_writer_sharing(options: &mut OpenOptions) {
    #[cfg(windows)]
    options.share_mode(FILE_SHARE_READ);
    #[cfg(not(windows))]
    let _ = options;
}

pub(crate) fn resolve_session_path(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    if path.exists() {
        fs::canonicalize(path)
    } else {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if is_reparse_metadata(&metadata) {
                return Err(reparse_error(path));
            }
        }
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "session path has no file name")
        })?;
        Ok(fs::canonicalize(parent)?.join(file_name))
    }
}

pub(crate) fn open_checked(mut options: OpenOptions, path: &Path) -> io::Result<File> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if is_reparse_metadata(&metadata) {
            return Err(reparse_error(path));
        }
    }
    #[cfg(windows)]
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    if is_reparse_metadata(&file.metadata()?) {
        return Err(reparse_error(path));
    }
    Ok(file)
}

fn is_reparse_metadata(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn reparse_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "reparse-point session path is not allowed: {}",
            path.display()
        ),
    )
}
