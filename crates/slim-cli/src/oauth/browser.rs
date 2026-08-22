use super::OAuthError;

pub trait BrowserLauncher: Send + Sync {
    fn open(&self, url: &str) -> Result<(), OAuthError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemBrowser;

impl BrowserLauncher for SystemBrowser {
    fn open(&self, url: &str) -> Result<(), OAuthError> {
        open_system_browser(url)
    }
}

#[cfg(windows)]
fn open_system_browser(url: &str) -> Result<(), OAuthError> {
    use std::iter::once;
    use std::ptr;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let operation = "open".encode_utf16().chain(once(0)).collect::<Vec<_>>();
    let url = url.encode_utf16().chain(once(0)).collect::<Vec<_>>();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            url.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result <= 32 {
        Err(OAuthError::Browser)
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn open_system_browser(_url: &str) -> Result<(), OAuthError> {
    Err(OAuthError::Browser)
}
