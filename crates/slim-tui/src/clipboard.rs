use std::io;

pub fn copy_text(text: &str) -> io::Result<()> {
    copy_text_platform(text)
}

#[cfg(windows)]
fn copy_text_platform(text: &str) -> io::Result<()> {
    use std::ptr::copy_nonoverlapping;
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
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
        struct ClipboardGuard;
        impl Drop for ClipboardGuard {
            fn drop(&mut self) {
                unsafe {
                    CloseClipboard();
                }
            }
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

#[cfg(not(windows))]
fn copy_text_platform(_text: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clipboard is unavailable on this platform",
    ))
}
