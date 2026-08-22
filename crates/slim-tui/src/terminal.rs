use std::io::{self, Stdout, Write};

use crossterm::cursor::Show;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

pub struct TerminalGuard {
    active: bool,
    #[cfg(windows)]
    /// Saved (output_cp, mode) restored on exit (spec §22.3 step 5).
    console: Option<(u32, u32)>,
}

#[cfg(windows)]
mod console {
    use std::io;

    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetConsoleOutputCP, GetStdHandle, SetConsoleCP, SetConsoleMode,
        SetConsoleOutputCP, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };

    /// Spec §22.1 step 2: activate UTF-8 + VT output before raw mode.
    pub fn setup() -> io::Result<(u32, u32)> {
        // Safety: plain FFI calls on the process console; no invariants.
        let prev_cp = unsafe { GetConsoleOutputCP() };
        unsafe {
            SetConsoleOutputCP(65001);
            SetConsoleCP(65001);
        }
        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let mut mode = 0u32;
        // VT may already be enabled (Windows Terminal); preserve the mode so
        // restore puts back exactly what was there.
        let prev_mode = unsafe {
            if GetConsoleMode(handle, &mut mode) != 0 {
                SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
                mode
            } else {
                u32::MAX // not a console (redirected); nothing to restore
            }
        };
        Ok((prev_cp, prev_mode))
    }

    pub fn restore(prev_cp: u32, prev_mode: u32) {
        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if prev_mode != u32::MAX {
            unsafe {
                SetConsoleMode(handle, prev_mode);
                SetConsoleOutputCP(prev_cp);
                SetConsoleCP(prev_cp);
            }
        }
    }
}

impl TerminalGuard {
    pub fn enter(stdout: &mut Stdout) -> io::Result<Self> {
        #[cfg(windows)]
        let console = console::setup()?;
        enable_raw_mode()?;
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            #[cfg(windows)]
            console::restore(console.0, console.1);
            return Err(error);
        }
        Ok(Self {
            active: true,
            #[cfg(windows)]
            console: Some(console),
        })
    }

    pub fn restore<W: Write>(&mut self, stdout: &mut W) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let leave_result = execute!(
            stdout,
            Show,
            LeaveAlternateScreen,
            DisableBracketedPaste,
            DisableMouseCapture
        );
        let raw_result = disable_raw_mode();
        #[cfg(windows)]
        if let Some((cp, mode)) = self.console {
            console::restore(cp, mode);
        }
        leave_result.and(raw_result)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let mut stdout = io::stdout();
            let _ = execute!(
                stdout,
                Show,
                LeaveAlternateScreen,
                DisableBracketedPaste,
                DisableMouseCapture
            );
            let _ = disable_raw_mode();
            #[cfg(windows)]
            if let Some((cp, mode)) = self.console {
                console::restore(cp, mode);
            }
        }
    }
}
