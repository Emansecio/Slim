use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::event_log::{data_path_from_handle, open_data_handle};
use super::schema_v2::{DurableRecord, DurableSessionHeader, MAX_DURABLE_SESSION_BYTES};

#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
#[cfg(windows)]
const FILE_SHARE_DELETE: u32 = 0x0000_0004;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};

/// A byte cursor owned by a read-only durable watch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DurableWatchCursor {
    offset: u64,
    last_seq: Option<u64>,
}

impl DurableWatchCursor {
    pub fn offset(self) -> u64 {
        self.offset
    }

    pub fn last_seq(self) -> Option<u64> {
        self.last_seq
    }
}

/// The result of one observational poll. A torn tail is reported, never
/// truncated, quarantined, repaired, or otherwise changed by the watch.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct DurableWatchBatch {
    records: Vec<DurableRecord>,
    cursor: DurableWatchCursor,
    torn_tail: bool,
}

impl DurableWatchBatch {
    pub fn records(&self) -> &[DurableRecord] {
        &self.records
    }

    pub fn cursor(&self) -> DurableWatchCursor {
        self.cursor
    }

    pub fn torn_tail(&self) -> bool {
        self.torn_tail
    }
}

impl std::fmt::Debug for DurableWatchBatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurableWatchBatch")
            .field("record_count", &self.records.len())
            .field("cursor", &self.cursor)
            .field("torn_tail", &self.torn_tail)
            .finish()
    }
}

/// A read-only watcher pinned to the file handle opened at construction.
///
/// It deliberately does not acquire the repository lock and does not use the
/// repository's recovery parser. This makes polling safe alongside an active
/// writer and keeps observation from becoming an authority-changing action.
pub struct DurableWatch {
    path: PathBuf,
    file: File,
    identity: FileIdentity,
    header: Option<DurableSessionHeader>,
    cursor: DurableWatchCursor,
}

impl DurableWatch {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let requested_path = path.as_ref();
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        let file = match open_data_handle(options, requested_path) {
            Ok(file) => file,
            Err(_first_error) => {
                // An existing durable writer intentionally advertises read
                // sharing but not write sharing. Fall back to read sharing so
                // a watcher can attach without disturbing that writer; the
                // wider mode is retained when the watcher opens first.
                #[cfg(windows)]
                {
                    let mut fallback = OpenOptions::new();
                    fallback.read(true).share_mode(FILE_SHARE_READ);
                    open_data_handle(fallback, requested_path)?
                }
                #[cfg(not(windows))]
                {
                    return Err(_first_error);
                }
            }
        };
        let path = data_path_from_handle(&file, requested_path)?;
        let identity = file_identity(&file)?;
        let length = file.metadata()?.len();
        if length > MAX_DURABLE_SESSION_BYTES {
            return Err(invalid_data(
                "durable watch source exceeds the 64 MiB limit",
            ));
        }
        Ok(Self {
            path,
            file,
            identity,
            header: None,
            cursor: DurableWatchCursor::default(),
        })
    }

    /// Reattach a watcher at a previously returned cursor. The prefix is
    /// scanned read-only to re-establish the session header and sequence
    /// guard; no records are emitted until the next poll.
    pub fn open_from_cursor(
        path: impl AsRef<Path>,
        cursor: DurableWatchCursor,
    ) -> io::Result<Self> {
        let mut watch = Self::open(path)?;
        let length = watch.file.metadata()?.len();
        if cursor.offset > length {
            return Err(invalid_data("durable watch cursor is beyond the source"));
        }
        watch.file.seek(SeekFrom::Start(0))?;
        let byte_count = usize::try_from(cursor.offset)
            .map_err(|_| invalid_data("durable watch cursor is too large"))?;
        let mut bytes = vec![0u8; byte_count];
        watch.file.read_exact(&mut bytes)?;
        let mut header = None;
        let mut last_seq = None;
        let mut consumed = 0usize;
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            if !line.ends_with(b"\n") {
                return Err(invalid_data("durable watch cursor splits a record"));
            }
            let value = serde_json::from_slice::<Value>(line)
                .map_err(|_| invalid_data("durable watch source contains invalid JSON"))?;
            let record_type = value
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_data("durable watch record type is invalid"))?;
            if header.is_none() {
                if record_type != "session" {
                    return Err(invalid_data(
                        "durable watch source is missing its session header",
                    ));
                }
                header = Some(
                    serde_json::from_value::<DurableSessionHeader>(value)
                        .map_err(|_| invalid_data("durable watch session header is invalid"))?,
                );
            } else {
                if record_type == "session" {
                    return Err(invalid_data(
                        "durable watch source has a duplicate session header",
                    ));
                }
                let record = serde_json::from_value::<DurableRecord>(value)
                    .map_err(|_| invalid_data("durable watch record is invalid"))?;
                if last_seq.is_some_and(|previous| record.seq() <= previous) {
                    return Err(invalid_data(
                        "durable watch record sequence is not increasing",
                    ));
                }
                last_seq = Some(record.seq());
            }
            consumed = consumed
                .checked_add(line.len())
                .ok_or_else(|| invalid_data("durable watch cursor overflowed"))?;
        }
        if consumed as u64 != cursor.offset || last_seq != cursor.last_seq {
            return Err(invalid_data(
                "durable watch cursor does not match the source",
            ));
        }
        watch.header = header;
        watch.cursor = cursor;
        Ok(watch)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn cursor(&self) -> DurableWatchCursor {
        self.cursor
    }

    pub fn header(&self) -> Option<&DurableSessionHeader> {
        self.header.as_ref()
    }

    /// Read only complete newline-terminated lines after the current cursor.
    /// A final incomplete line is returned as `torn_tail` and remains pending.
    pub fn poll(&mut self) -> io::Result<DurableWatchBatch> {
        let length = self.file.metadata()?.len();
        if length > MAX_DURABLE_SESSION_BYTES {
            return Err(invalid_data(
                "durable watch source exceeds the 64 MiB limit",
            ));
        }
        if length < self.cursor.offset {
            return Err(invalid_data("durable watch source was truncated"));
        }

        let remaining_limit = MAX_DURABLE_SESSION_BYTES
            .checked_sub(self.cursor.offset)
            .ok_or_else(|| invalid_data("durable watch cursor exceeds the 64 MiB limit"))?;
        self.file.seek(SeekFrom::Start(self.cursor.offset))?;
        let bytes = read_bounded(
            &mut self.file,
            remaining_limit,
            length.saturating_sub(self.cursor.offset),
        )?;
        // The file may have grown after the first metadata check. Refuse the
        // oversized observation before any JSON parsing or state mutation.
        let after_length = self.file.metadata()?.len();
        if after_length > MAX_DURABLE_SESSION_BYTES {
            return Err(invalid_data(
                "durable watch source exceeds the 64 MiB limit",
            ));
        }
        let observed_end = self
            .cursor
            .offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid_data("durable watch cursor overflowed"))?;
        if observed_end > after_length {
            return Err(invalid_data("durable watch source was truncated"));
        }
        let mut cursor = self.cursor;
        let mut header = self.header.clone();
        let mut records = Vec::new();
        let mut torn_tail = false;
        let mut consumed = 0usize;

        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            if !line.ends_with(b"\n") {
                torn_tail = true;
                break;
            }
            let value = serde_json::from_slice::<Value>(line)
                .map_err(|_| invalid_data("durable watch source contains invalid JSON"))?;
            let record_type = value
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_data("durable watch record type is invalid"))?;
            if header.is_none() {
                if record_type != "session" {
                    return Err(invalid_data(
                        "durable watch source is missing its session header",
                    ));
                }
                header = Some(
                    serde_json::from_value::<DurableSessionHeader>(value)
                        .map_err(|_| invalid_data("durable watch session header is invalid"))?,
                );
            } else {
                if record_type == "session" {
                    return Err(invalid_data(
                        "durable watch source has a duplicate session header",
                    ));
                }
                let record = serde_json::from_value::<DurableRecord>(value)
                    .map_err(|_| invalid_data("durable watch record is invalid"))?;
                if cursor
                    .last_seq
                    .is_some_and(|previous| record.seq() <= previous)
                {
                    return Err(invalid_data(
                        "durable watch record sequence is not increasing",
                    ));
                }
                cursor.last_seq = Some(record.seq());
                records.push(record);
            }
            consumed = consumed
                .checked_add(line.len())
                .ok_or_else(|| invalid_data("durable watch cursor overflowed"))?;
        }

        if consumed < bytes.len() {
            torn_tail = true;
        }
        cursor.offset = cursor
            .offset
            .checked_add(consumed as u64)
            .ok_or_else(|| invalid_data("durable watch cursor overflowed"))?;
        self.cursor = cursor;
        self.header = header;
        Ok(DurableWatchBatch {
            records,
            cursor,
            torn_tail,
        })
    }

    /// The identity is exposed only as a stable opaque pair for diagnostics;
    /// it never includes path contents or file bytes.
    pub fn identity(&self) -> (u64, u64) {
        (self.identity.0, self.identity.1)
    }
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

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_bounded<R: Read>(
    reader: &mut R,
    max_bytes: u64,
    initial_available: u64,
) -> io::Result<Vec<u8>> {
    let read_limit = max_bytes
        .checked_add(1)
        .ok_or_else(|| invalid_data("durable watch read limit overflowed"))?;
    let capacity = usize::try_from(initial_available.min(read_limit))
        .map_err(|_| invalid_data("durable watch read limit is too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
    reader.take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(invalid_data(
            "durable watch observation exceeds the 64 MiB limit",
        ));
    }
    Ok(bytes)
}

// Keep aliases short for callers that use the generic watch vocabulary.
pub type WatchBatch = DurableWatchBatch;
pub type WatchCursor = DurableWatchCursor;

#[cfg(test)]
mod tests {
    use super::read_bounded;
    use std::io::{self, Read};

    struct GrowingReader {
        bytes: Vec<u8>,
        offset: usize,
        grew: bool,
    }

    impl Read for GrowingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.grew {
                self.bytes.extend_from_slice(b"growth-after-metadata");
                self.grew = true;
            }
            if self.offset == self.bytes.len() {
                return Ok(0);
            }
            let count = buffer.len().min(self.bytes.len() - self.offset);
            buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }

    #[test]
    fn bounded_reader_rejects_growth_before_parsing() {
        let mut reader = GrowingReader {
            bytes: b"ok".to_vec(),
            offset: 0,
            grew: false,
        };
        let error = read_bounded(&mut reader, 4, 2).expect_err("growth must be bounded");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
