//! Real-clipboard coverage for the composer's Ctrl+V image path. The cases
//! need an interactive desktop, so they are ignored by default like the PTY
//! host tests.
#![cfg(windows)]

use slim_tui::bitmap;
use slim_tui::clipboard;
use windows_sys::Win32::System::Ole::CF_DIB;

/// 2x2 24-bit bottom-up DIB: bottom row red/green, top row blue/white.
fn fixture_dib() -> Vec<u8> {
    let mut dib = vec![0u8; 40];
    dib[0..4].copy_from_slice(&40u32.to_le_bytes());
    dib[4..8].copy_from_slice(&2i32.to_le_bytes());
    dib[8..12].copy_from_slice(&2i32.to_le_bytes());
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&24u16.to_le_bytes());
    dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
    dib.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]);
    dib
}

fn set_clipboard_format(format: u32, bytes: &[u8]) {
    use std::ptr::copy_nonoverlapping;
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };

    unsafe {
        assert_ne!(OpenClipboard(std::ptr::null_mut()), 0, "open clipboard");
        assert_ne!(EmptyClipboard(), 0, "empty clipboard");
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        assert!(!memory.is_null(), "allocate clipboard payload");
        let target = GlobalLock(memory).cast::<u8>();
        assert!(!target.is_null(), "lock clipboard payload");
        copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
        GlobalUnlock(memory);
        if SetClipboardData(format, memory as *mut _).is_null() {
            GlobalFree(memory);
            panic!("set clipboard payload");
        }
        CloseClipboard();
    }
}

fn registered_png_format() -> u32 {
    use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;

    let name: Vec<u16> = "PNG\0".encode_utf16().collect();
    unsafe { RegisterClipboardFormatW(name.as_ptr()) }
}

fn restore_clipboard_text(previous: Option<String>) {
    if let Some(text) = previous.filter(|text| !text.is_empty()) {
        let _ = clipboard::copy_text(&text);
    }
}

#[test]
#[ignore = "requires an interactive desktop clipboard"]
fn dib_clipboard_image_becomes_a_png_attachment() {
    let previous = clipboard::paste_text().ok();
    set_clipboard_format(u32::from(CF_DIB), &fixture_dib());

    let content = clipboard::pull().expect("pull a DIB clipboard");
    let path = content.image_path.expect("pasted DIB yields a file");
    let png = std::fs::read(&path).expect("read pasted png");
    assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    assert_eq!(
        u32::from_be_bytes(png[16..20].try_into().unwrap()),
        2,
        "IHDR width"
    );
    assert_eq!(
        u32::from_be_bytes(png[20..24].try_into().unwrap()),
        2,
        "IHDR height"
    );
    assert!(path.contains("slim-paste-"), "temp paste directory: {path}");

    // A text-only clipboard keeps the historical text path.
    let _ = clipboard::copy_text("slim-clipboard-fixture");
    let text = clipboard::pull().expect("pull a text clipboard");
    assert_eq!(text.image_path, None);
    assert_eq!(text.text.as_deref(), Some("slim-clipboard-fixture"));

    let _ = std::fs::remove_file(&path);
    restore_clipboard_text(previous);
}

#[test]
#[ignore = "requires an interactive desktop clipboard"]
fn png_clipboard_format_is_reused_without_re_encoding() {
    let previous = clipboard::paste_text().ok();
    let png = bitmap::png_from_dib(&fixture_dib()).expect("encode fixture");
    set_clipboard_format(registered_png_format(), &png);

    let content = clipboard::pull().expect("pull a PNG clipboard");
    let path = content.image_path.expect("pasted PNG yields a file");
    assert_eq!(
        std::fs::read(&path).expect("read pasted png"),
        png,
        "an application PNG is attached byte for byte"
    );

    let _ = std::fs::remove_file(&path);
    restore_clipboard_text(previous);
}
