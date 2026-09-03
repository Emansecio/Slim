use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use std::cell::Cell;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};

use serde_json::Value;

use super::event_log::{
    acquire_lock, configure_data_writer_sharing, data_path_from_handle, open_checked,
    open_data_handle, resolve_session_path,
};
use super::recovery::quarantine_torn_tail;
use super::repository::{validate_next, DurableAppendValidator, DurableRepo};
use super::schema_v2::{DurableRecord, DurableSessionHeader, MAX_DURABLE_SESSION_BYTES};

#[cfg(test)]
thread_local! {
    static APPEND_SYNC_CALLS: Cell<u64> = const { Cell::new(0) };
}

pub struct JsonlRepo {
    path: PathBuf,
    file: File,
    _lock: File,
    header: DurableSessionHeader,
    records: Vec<DurableRecord>,
    validator: DurableAppendValidator,
    poisoned: bool,
}

impl JsonlRepo {
    pub fn create(path: impl AsRef<Path>, header: DurableSessionHeader) -> io::Result<Self> {
        let path = resolve_session_path(path.as_ref())?;
        Self::create_impl(path, header)
    }

    fn create_impl(path: PathBuf, header: DurableSessionHeader) -> io::Result<Self> {
        let lock = acquire_lock(&path)?;
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "durable session data file already exists",
            ));
        }

        let (temp_path, mut file) = create_temp_file(&path)?;
        let result = (|| {
            let encoded = encode_line(&header)?;
            ensure_line_fits(0, encoded.len())?;
            write_line(&mut file, &encoded)?;
            file.sync_data()?;
            let temp_identity = file_identity(&file)?;
            publish_temp_file(&file, &temp_path, &path)?;
            if file_identity(&file)? != temp_identity {
                return Err(invalid_data("durable session file identity changed"));
            }
            file.seek(SeekFrom::End(0))?;
            Ok(())
        })();
        if let Err(error) = result {
            drop(file);
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }

        Ok(Self {
            path,
            file,
            _lock: lock,
            header,
            records: Vec::new(),
            validator: DurableAppendValidator::empty(),
            poisoned: false,
        })
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_impl(path, |_, _| Ok(()))
    }

    /// Open a v2 repository without repairing a torn tail or inserting a
    /// missing separator. This is the safe handoff for resume callers after a
    /// read-only preflight: a changed or incomplete file fails closed and its
    /// bytes remain untouched.
    pub fn open_no_repair(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_impl_mode(path, |_, _| Ok(()), false, None)
    }

    /// Open a healthy v2 prefix without repair and require that the pinned
    /// handle still contains the exact header and records observed by a prior
    /// read-only preflight. This closes the preflight-to-open replacement
    /// window for resume callers.
    pub fn open_no_repair_expected(
        path: impl AsRef<Path>,
        expected_header: &DurableSessionHeader,
        expected_records: &[DurableRecord],
    ) -> io::Result<Self> {
        Self::open_impl_mode(
            path,
            |_, _| Ok(()),
            false,
            Some((expected_header, expected_records)),
        )
    }

    fn open_impl<F>(path: impl AsRef<Path>, before_path: F) -> io::Result<Self>
    where
        F: FnOnce(&File, &Path) -> io::Result<()>,
    {
        Self::open_impl_mode(path, before_path, true, None)
    }

    fn open_impl_mode<F>(
        path: impl AsRef<Path>,
        before_path: F,
        repair: bool,
        expected: Option<(&DurableSessionHeader, &[DurableRecord])>,
    ) -> io::Result<Self>
    where
        F: FnOnce(&File, &Path) -> io::Result<()>,
    {
        let requested_path = path.as_ref();
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        configure_data_writer_sharing(&mut options);
        let mut file = open_data_handle(options, requested_path)?;
        before_path(&file, requested_path)?;
        let path = data_path_from_handle(&file, requested_path)?;
        let lock = acquire_lock(&path)?;
        let parsed = parse(&mut file)?;
        let header = parsed
            .header
            .ok_or_else(|| invalid_data("missing session header"))?;
        let validator = DurableAppendValidator::from_records(&parsed.records)?;
        if let Some((expected_header, expected_records)) = expected {
            if &header != expected_header || parsed.records != expected_records {
                return Err(invalid_data(
                    "durable session changed after read-only preflight",
                ));
            }
        }

        if let Some(tail) = parsed.torn_tail.as_deref() {
            if !repair {
                return Err(invalid_data(
                    "durable session has a torn tail; explicit recovery is required",
                ));
            }
            quarantine_torn_tail(&path, tail)?;
            file.set_len(parsed.valid_offset as u64)?;
            file.sync_data()?;
        } else if parsed.needs_separator {
            if !repair {
                return Err(invalid_data(
                    "durable session is missing a separator; explicit recovery is required",
                ));
            }
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
            file.sync_data()?;
        }
        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            path,
            file,
            _lock: lock,
            header,
            records: parsed.records,
            validator,
            poisoned: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the only sequence accepted for the next append without
    /// mutating the repository. A maxed-out prefix has no representable
    /// successor and therefore fails closed.
    pub fn next_seq(&self) -> io::Result<u64> {
        self.records
            .last()
            .map(|record| {
                record.seq().checked_add(1).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "durable session sequence overflowed",
                    )
                })
            })
            .unwrap_or(Ok(0))
    }
}

impl DurableRepo for JsonlRepo {
    fn header(&self) -> &DurableSessionHeader {
        &self.header
    }

    fn records(&self) -> &[DurableRecord] {
        &self.records
    }

    fn append(&mut self, record: DurableRecord) -> io::Result<()> {
        self.append_batch(vec![record])
    }

    fn append_batch(&mut self, records: Vec<DurableRecord>) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("durable session repository is poisoned"));
        }
        if records.is_empty() {
            return Ok(());
        }
        let prepared = self.validator.prepare_batch(&self.records, &records)?;
        let mut encoded_records = Vec::with_capacity(records.len());
        for record in &records {
            match encode_line(record) {
                Ok(encoded) => encoded_records.push(encoded),
                Err(error) => {
                    self.poisoned = true;
                    return Err(error);
                }
            }
        }
        let mut final_len = self.file.metadata()?.len();
        for encoded in &encoded_records {
            ensure_line_fits(final_len, encoded.len())?;
            final_len = final_len
                .checked_add(encoded.len() as u64)
                .and_then(|length| length.checked_add(1))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "durable session size overflowed",
                    )
                })?;
        }
        let write_result = encoded_records
            .iter()
            .try_for_each(|encoded| write_line(&mut self.file, encoded))
            .and_then(|_| sync_append_data(&self.file));
        if let Err(error) = write_result {
            self.poisoned = true;
            return Err(error);
        }
        self.validator.commit(prepared);
        self.records.extend(records);
        Ok(())
    }
}

pub(crate) struct ParsedJsonl {
    pub(crate) header: Option<DurableSessionHeader>,
    pub(crate) records: Vec<DurableRecord>,
    pub(crate) valid_offset: usize,
    pub(crate) torn_tail: Option<Vec<u8>>,
    pub(crate) needs_separator: bool,
}

pub(crate) fn parse(file: &mut File) -> io::Result<ParsedJsonl> {
    let file_len = file.metadata()?.len();
    if file_len > MAX_DURABLE_SESSION_BYTES {
        return Err(invalid_data(
            "durable session file exceeds the 64 MiB limit",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut offset = 0usize;
    let mut header = None;
    let mut records = Vec::new();
    let mut torn_tail = None;
    let mut needs_separator = false;

    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.ends_with(b"\n");
        let is_final_line = offset + line.len() == bytes.len();
        let value = match serde_json::from_slice::<Value>(line) {
            Ok(value) => value,
            Err(error) if error.is_eof() && !has_newline && is_final_line => {
                torn_tail = Some(bytes[offset..].to_vec());
                break;
            }
            Err(_) => return Err(invalid_data("invalid durable session JSON")),
        };

        let record_type = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_data("durable session record type is invalid"))?;
        if header.is_none() {
            if record_type != "session" {
                return Err(invalid_data("record before session header"));
            }
            header = Some(
                serde_json::from_value::<DurableSessionHeader>(value).map_err(invalid_data_from)?,
            );
        } else if record_type == "session" {
            return Err(invalid_data("duplicate session header"));
        } else {
            let record =
                serde_json::from_value::<DurableRecord>(value).map_err(invalid_data_from)?;
            validate_next(&records, record.seq())
                .map_err(|_| invalid_data("record sequence is not increasing"))?;
            records.push(record);
        }

        if is_final_line && !has_newline {
            needs_separator = true;
        }
        offset += line.len();
    }

    Ok(ParsedJsonl {
        header,
        records,
        valid_offset: offset,
        torn_tail,
        needs_separator,
    })
}

fn encode_line<T: serde::Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(io::Error::other)
}

fn write_line(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn sync_append_data(file: &File) -> io::Result<()> {
    #[cfg(test)]
    APPEND_SYNC_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    file.sync_data()
}

#[cfg(test)]
fn take_append_sync_calls() -> u64 {
    APPEND_SYNC_CALLS.with(|calls| {
        let value = calls.get();
        calls.set(0);
        value
    })
}

fn ensure_line_fits(current_len: u64, encoded_len: usize) -> io::Result<()> {
    let encoded_len = u64::try_from(encoded_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable JSON line is too large",
        )
    })?;
    let final_len = current_len
        .checked_add(encoded_len)
        .and_then(|length| length.checked_add(1))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable session size overflowed",
            )
        })?;
    if final_len > MAX_DURABLE_SESSION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable session would exceed the 64 MiB limit",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity(u64, u64);

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct WinFileTime {
    low: u32,
    high: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: WinFileTime,
    last_access_time: WinFileTime,
    last_write_time: WinFileTime,
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    #[link_name = "GetFileInformationByHandle"]
    fn get_file_information_by_handle(
        file: RawHandle,
        information: *mut ByHandleFileInformation,
    ) -> i32;

    #[link_name = "SetFileInformationByHandle"]
    fn set_file_information_by_handle(
        file: RawHandle,
        information_class: u32,
        information: *mut std::ffi::c_void,
        buffer_size: u32,
    ) -> i32;
}

fn file_identity(file: &File) -> io::Result<FileIdentity> {
    #[cfg(windows)]
    {
        let mut information = ByHandleFileInformation::default();
        let result =
            unsafe { get_file_information_by_handle(file.as_raw_handle(), &mut information) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileIdentity(
            u64::from(information.volume_serial_number),
            (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low),
        ))
    }
    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        Ok(FileIdentity(metadata.dev(), metadata.ino()))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let metadata = file.metadata()?;
        Ok(FileIdentity(metadata.len(), metadata.len()))
    }
}

fn create_temp_file(path: &Path) -> io::Result<(PathBuf, File)> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "session path has no file name")
    })?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let base = path.with_file_name(format!(
        ".{}.tmp-{}-{stamp}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    for index in 0.. {
        let candidate = if index == 0 {
            base.clone()
        } else {
            PathBuf::from(format!("{}.{}", base.display(), index))
        };
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        configure_temp_file_access(&mut options);
        match open_checked(options, &candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!("temporary path suffix space exhausted")
}

#[cfg(windows)]
fn configure_temp_file_access(options: &mut OpenOptions) {
    const DELETE: u32 = 0x0001_0000;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    options.access_mode(GENERIC_READ | GENERIC_WRITE | DELETE);
    configure_data_writer_sharing(options);
}

#[cfg(not(windows))]
fn configure_temp_file_access(_options: &mut OpenOptions) {}

#[cfg(windows)]
fn publish_temp_file(file: &File, _temp_path: &Path, final_path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct FileRenameInfoHeader {
        replace_if_exists: u32,
        root_directory: RawHandle,
        file_name_length: u32,
        file_name: [u16; 1],
    }

    const FILE_RENAME_INFO: u32 = 3;
    let wide_name: Vec<u16> = final_path.as_os_str().encode_wide().collect();
    let file_name_length = wide_name
        .len()
        .checked_mul(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename path is too long"))?;
    // The backing payload includes a terminator; FileNameLength below excludes it.
    let name_bytes = file_name_length
        .checked_add(std::mem::size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename path is too long"))?;
    let header_bytes = std::mem::offset_of!(FileRenameInfoHeader, file_name);
    let total_bytes = header_bytes
        .checked_add(name_bytes)
        .map(|bytes| bytes.max(std::mem::size_of::<FileRenameInfoHeader>()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename path is too long"))?;
    let word_count = total_bytes.div_ceil(std::mem::size_of::<usize>());
    let mut storage = vec![0usize; word_count];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            storage.as_mut_ptr().cast::<u8>(),
            storage.len() * std::mem::size_of::<usize>(),
        )
    };
    let file_name_length_offset = std::mem::offset_of!(FileRenameInfoHeader, file_name_length);
    bytes[file_name_length_offset..file_name_length_offset + 4].copy_from_slice(
        &u32::try_from(file_name_length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rename path is too long"))?
            .to_ne_bytes(),
    );
    for (index, unit) in wide_name.iter().copied().enumerate() {
        let start = header_bytes + index * std::mem::size_of::<u16>();
        bytes[start..start + std::mem::size_of::<u16>()].copy_from_slice(&unit.to_ne_bytes());
    }
    let buffer_size = u32::try_from(total_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rename path is too long"))?;
    let result = unsafe {
        set_file_information_by_handle(
            file.as_raw_handle(),
            FILE_RENAME_INFO,
            bytes.as_mut_ptr().cast(),
            buffer_size,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn publish_temp_file(file: &File, temp_path: &Path, final_path: &Path) -> io::Result<()> {
    let _ = file;
    // A same-filesystem hard link publishes without replacing a raced destination;
    // unlinking the temporary name leaves the already-open inode at the final path.
    fs::hard_link(temp_path, final_path)?;
    fs::remove_file(temp_path)
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_data_from(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path() -> PathBuf {
        let root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let candidate = root.join(format!(
            "slim-jsonl-repo-identity-{}-{stamp}",
            std::process::id()
        ));
        assert_eq!(candidate.parent(), Some(root.as_path()));
        fs::create_dir(&candidate).expect("create unique test directory");
        let directory = fs::canonicalize(&candidate).expect("canonical test directory");
        assert_eq!(directory.parent(), Some(root.as_path()));
        directory.join("session.jsonl")
    }

    #[test]
    fn publish_keeps_the_original_handle_published() {
        let requested = test_path();
        let path = resolve_session_path(&requested).expect("resolve path");
        let (temp_path, mut file) = create_temp_file(&path).expect("create temp file");
        let header = DurableSessionHeader::new("identity", "now", "D:\\Slim", None, None);
        let encoded = encode_line(&header).expect("encode header");
        write_line(&mut file, &encoded).expect("write header");
        file.sync_data().expect("sync header");
        let identity = file_identity(&file).expect("temp identity");

        publish_temp_file(&file, &temp_path, &path).expect("publish by handle");

        assert_eq!(file_identity(&file).expect("published identity"), identity);
        assert!(!temp_path.exists(), "published temp path must be gone");
        file.seek(SeekFrom::End(0)).expect("seek published handle");
        assert_eq!(fs::read(&path).expect("read published data"), {
            let mut expected = encoded;
            expected.push(b'\n');
            expected
        });
        drop(file);
        fs::remove_dir_all(path.parent().expect("test directory")).expect("remove test directory");
    }

    #[test]
    fn append_batch_syncs_once_for_multiple_records() {
        let requested = test_path();
        let mut repo = JsonlRepo::create(
            &requested,
            DurableSessionHeader::new("batch-sync", "now", "D:\\Slim", None, None),
        )
        .expect("create repo");
        take_append_sync_calls();

        repo.append_batch(
            (0..3)
                .map(|seq| DurableRecord::Fact {
                    seq,
                    fact: super::super::schema_v2::DurableFact {
                        namespace: "batch".into(),
                        key: format!("k-{seq}"),
                        value: serde_json::Value::Null,
                    },
                })
                .collect(),
        )
        .expect("append batch");

        assert_eq!(take_append_sync_calls(), 1);
        assert_eq!(repo.records().len(), 3);
        drop(repo);
        fs::remove_dir_all(requested.parent().expect("test directory")).expect("cleanup");
    }

    #[test]
    fn publish_does_not_replace_an_existing_destination() {
        let requested = test_path();
        let path = resolve_session_path(&requested).expect("resolve path");
        let (temp_path, mut file) = create_temp_file(&path).expect("create temp file");
        let existing = b"existing destination";
        fs::write(&path, existing).expect("write existing destination");
        write_line(&mut file, b"temporary").expect("write temp");
        file.sync_data().expect("sync temp");

        let error = publish_temp_file(&file, &temp_path, &path)
            .expect_err("existing destination must reject publication");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
        ));
        assert_eq!(
            fs::read(&path).expect("read existing destination"),
            existing
        );
        drop(file);
        let _ = fs::remove_file(&temp_path);
        fs::remove_dir_all(path.parent().expect("test directory")).expect("remove test directory");
    }

    #[test]
    fn append_rejects_size_overflow_without_changing_bytes_or_state() {
        let requested = test_path();
        let mut repo = JsonlRepo::create(
            &requested,
            DurableSessionHeader::new("overflow", "now", "D:\\Slim", None, None),
        )
        .expect("create repository");
        repo.file
            .set_len(MAX_DURABLE_SESSION_BYTES)
            .expect("make bounded-size file");
        repo.file
            .seek(SeekFrom::End(0))
            .expect("seek bounded-size file");
        let before_len = repo.file.metadata().expect("metadata").len();
        let before_records = repo.records.clone();
        let record = DurableRecord::Fact {
            seq: 0,
            fact: super::super::schema_v2::DurableFact {
                namespace: "ns".into(),
                key: "key".into(),
                value: serde_json::Value::Null,
            },
        };
        let error = repo.append(record).expect_err("size overflow must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            repo.file.metadata().expect("metadata after").len(),
            before_len
        );
        assert_eq!(repo.records, before_records);
        drop(repo);
        fs::remove_dir_all(requested.parent().expect("test directory")).expect("cleanup");
    }

    #[test]
    fn append_does_not_rescan_prefix_and_open_validates_once() {
        use super::super::repository::take_full_prefix_validations;

        let _ = take_full_prefix_validations();
        let requested = test_path();
        let mut repo = JsonlRepo::create(
            &requested,
            DurableSessionHeader::new("incremental", "now", "D:\\Slim", None, None),
        )
        .expect("create repository");
        assert_eq!(take_full_prefix_validations(), 0);
        for seq in 0..24 {
            repo.append(DurableRecord::Fact {
                seq,
                fact: super::super::schema_v2::DurableFact {
                    namespace: "ns".into(),
                    key: format!("k-{seq}"),
                    value: serde_json::Value::Null,
                },
            })
            .expect("append fact");
        }
        assert_eq!(
            take_full_prefix_validations(),
            0,
            "jsonl append must not revalidate the whole prefix"
        );
        let before = repo.records.clone();
        let error = repo
            .append(DurableRecord::Fact {
                seq: 0,
                fact: super::super::schema_v2::DurableFact {
                    namespace: "ns".into(),
                    key: "dup".into(),
                    value: serde_json::Value::Null,
                },
            })
            .expect_err("duplicate sequence");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(repo.records, before);
        drop(repo);

        JsonlRepo::open(&requested).expect("open reconstructed prefix");
        assert_eq!(take_full_prefix_validations(), 1);
        fs::remove_dir_all(requested.parent().expect("test directory")).expect("cleanup");
    }

    #[test]
    fn create_rejects_oversized_header_before_publishing() {
        let requested = test_path();
        let header = DurableSessionHeader::new(
            "x".repeat((MAX_DURABLE_SESSION_BYTES as usize) + 1),
            "now",
            "D:\\Slim",
            None,
            None,
        );
        let error = match JsonlRepo::create(&requested, header) {
            Ok(_) => panic!("oversized header must fail before publish"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            !requested.exists(),
            "oversized header must not publish data"
        );
        fs::remove_dir_all(requested.parent().expect("test directory")).expect("cleanup");
    }

    #[cfg(windows)]
    #[test]
    fn open_derives_pinned_target_path_from_handle_before_alias_changes() {
        use std::os::windows::fs::symlink_file;

        let requested = test_path();
        let parent = requested.parent().expect("test directory");
        let target_a = requested.clone();
        let target_b = parent.join("other.jsonl");
        let alias = parent.join("alias.jsonl");
        let repo_a = JsonlRepo::create(
            &target_a,
            DurableSessionHeader::new("pinned-a", "now", "D:\\Slim", None, None),
        )
        .expect("create target a");
        drop(repo_a);
        let repo_b = JsonlRepo::create(
            &target_b,
            DurableSessionHeader::new("pinned-b", "now", "D:\\Slim", None, None),
        )
        .expect("create target b");
        drop(repo_b);
        match symlink_file(&target_a, &alias) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                fs::remove_dir_all(parent).expect("remove test directory");
                return;
            }
            Err(error) => panic!("symlink setup failed: {error}"),
        }

        let pinned = JsonlRepo::open_impl(&alias, |_, alias_path| {
            fs::remove_file(alias_path)?;
            symlink_file(&target_b, alias_path)
        })
        .expect("open pinned target");
        let canonical_a = fs::canonicalize(&target_a).expect("canonical target a");
        assert_eq!(pinned.path(), canonical_a.as_path());
        assert!(
            JsonlRepo::open(&target_b).is_ok(),
            "lock must remain on target a"
        );
        drop(pinned);
        fs::remove_dir_all(parent).expect("remove test directory");
    }
}
