//! Windows clipboard access. Text copy/paste plus the composer's image pull
//! (DESIGN-SLIM-TUI §20): a pasted bitmap is materialized as a PNG file in the
//! user temp directory so the existing `/image PATH` attachment pipeline can
//! load it. Stale pastes are pruned on the next paste.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Prefix of the per-process directory holding pasted bitmaps.
const PASTE_DIR_PREFIX: &str = "slim-paste-";
/// Terminal images are pastes, not documents: anything older than this is
/// garbage from a previous session.
const PASTE_TTL: Duration = Duration::from_secs(60 * 60);

static NEXT_PASTE: AtomicU64 = AtomicU64::new(0);

pub fn copy_text(text: &str) -> io::Result<()> {
    copy_text_platform(text)
}

pub fn paste_text() -> io::Result<String> {
    paste_text_platform()
}

/// One clipboard read for the composer paste path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClipboardContent {
    /// PNG file for `UiCommand::AttachImage`; `None` when the clipboard holds
    /// no usable raster image.
    pub image_path: Option<String>,
    /// Clipboard text, so a text-only target (login API key) still pastes.
    pub text: Option<String>,
}

/// Reads the clipboard once. `Err` means an image was present but could not be
/// materialized, and the message is user-facing.
pub fn pull() -> Result<ClipboardContent, String> {
    let image_path = paste_image_platform()?;
    let text = paste_text().ok().filter(|text| !text.is_empty());
    Ok(ClipboardContent { image_path, text })
}

#[cfg(windows)]
struct ClipboardGuard;

#[cfg(windows)]
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::DataExchange::CloseClipboard();
        }
    }
}

#[cfg(windows)]
fn copy_text_platform(text: &str) -> io::Result<()> {
    use std::ptr::copy_nonoverlapping;
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    let payload = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err(io::Error::last_os_error());
        }
        let _guard = ClipboardGuard;
        if EmptyClipboard() == 0 {
            return Err(io::Error::last_os_error());
        }
        let bytes = payload.len() * std::mem::size_of::<u16>();
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if memory.is_null() {
            return Err(io::Error::last_os_error());
        }
        let target = GlobalLock(memory).cast::<u16>();
        if target.is_null() {
            GlobalFree(memory);
            return Err(io::Error::last_os_error());
        }
        copy_nonoverlapping(payload.as_ptr(), target, payload.len());
        GlobalUnlock(memory);
        if SetClipboardData(u32::from(CF_UNICODETEXT), memory as *mut _).is_null() {
            GlobalFree(memory);
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn paste_text_platform() -> io::Result<String> {
    use windows_sys::Win32::System::DataExchange::{GetClipboardData, OpenClipboard};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err(io::Error::last_os_error());
        }
        let _guard = ClipboardGuard;
        let memory = GetClipboardData(u32::from(CF_UNICODETEXT));
        if memory.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "clipboard has no unicode text",
            ));
        }
        let source = GlobalLock(memory).cast::<u16>();
        if source.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0usize;
        while *source.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(source, len));
        GlobalUnlock(memory);
        Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
    }
}

/// Materializes a clipboard raster as a PNG file. `Ok(None)` keeps the text
/// path for clipboards without a readable image.
#[cfg(windows)]
fn paste_image_platform() -> Result<Option<String>, String> {
    use windows_sys::Win32::System::DataExchange::{
        IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    };
    use windows_sys::Win32::System::Ole::{CF_BITMAP, CF_DIB, CF_DIBV5};

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            // A busy clipboard keeps the historical text-only behavior instead
            // of failing a paste that may not involve any image.
            return Ok(None);
        }
        let _guard = ClipboardGuard;
        let png_format = RegisterClipboardFormatW(wide_null("PNG").as_ptr());
        if png_format != 0 && IsClipboardFormatAvailable(png_format) != 0 {
            let payload = clipboard_bytes(png_format)?;
            // An application-encoded PNG is already exactly what the loader
            // wants; only trim the allocation granule appended by the clipboard.
            if let Some(png) = crate::bitmap::trim_png(&payload) {
                return write_paste_file(png).map(Some);
            }
        }
        for format in [u32::from(CF_DIB), u32::from(CF_DIBV5)] {
            if IsClipboardFormatAvailable(format) != 0 {
                let payload = clipboard_bytes(format)?;
                let png = crate::bitmap::png_from_dib(&payload)
                    .map_err(|reason| format!("clipboard bitmap is not supported: {reason}"))?;
                return write_paste_file(&png).map(Some);
            }
        }
        if IsClipboardFormatAvailable(u32::from(CF_BITMAP)) != 0 {
            return Err(
                "clipboard holds a bitmap without a DIB payload; save it as a file and use /image PATH"
                    .into(),
            );
        }
        Ok(None)
    }
}

/// Copies a clipboard format payload out of the open clipboard.
///
/// # Safety
/// The clipboard must be open on this thread (callers hold `ClipboardGuard`).
#[cfg(windows)]
unsafe fn clipboard_bytes(format: u32) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::System::DataExchange::GetClipboardData;
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    let handle = GetClipboardData(format);
    if handle.is_null() {
        return Err("clipboard payload is unavailable".into());
    }
    let size = GlobalSize(handle);
    if size == 0 {
        return Err("clipboard payload is empty".into());
    }
    let source = GlobalLock(handle).cast::<u8>();
    if source.is_null() {
        return Err("clipboard payload cannot be locked".into());
    }
    let mut bytes = vec![0u8; size];
    std::ptr::copy_nonoverlapping(source, bytes.as_mut_ptr(), size);
    GlobalUnlock(handle);
    Ok(bytes)
}

#[cfg(windows)]
fn write_paste_file(png: &[u8]) -> Result<String, String> {
    let directory = paste_directory();
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    prune_stale_paste_directories(&directory);
    let path = directory.join(format!("clipboard-{}.png", next_paste_id()));
    std::fs::write(&path, png)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}

fn paste_directory() -> PathBuf {
    std::env::temp_dir().join(format!("{PASTE_DIR_PREFIX}{}", std::process::id()))
}

fn next_paste_id() -> u64 {
    NEXT_PASTE.fetch_add(1, Ordering::Relaxed)
}

/// Best-effort cleanup of pasted bitmaps left by earlier sessions.
fn prune_stale_paste_directories(current: &Path) {
    let Some(entries) = current
        .parent()
        .and_then(|parent| std::fs::read_dir(parent).ok())
    else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let names_a_paste = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(PASTE_DIR_PREFIX));
        if !names_a_paste || path == current {
            continue;
        }
        if entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age > PASTE_TTL)
            })
        {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[cfg(windows)]
fn wide_null(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(not(windows))]
fn paste_image_platform() -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(not(windows))]
fn copy_text_platform(_text: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clipboard is unavailable on this platform",
    ))
}

#[cfg(not(windows))]
fn paste_text_platform() -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clipboard is unavailable on this platform",
    ))
}
