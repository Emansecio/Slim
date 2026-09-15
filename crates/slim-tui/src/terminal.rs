use std::io::{self, Stdout, Write};

use crossterm::cursor::Show;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

pub struct TerminalGuard {
    active: bool,
    mouse_capture: bool,
    #[cfg(windows)]
    /// Saved (output_cp, mode) restored on exit (spec §22.3 step 5).
    console: Option<console::ConsoleState>,
}

#[cfg(windows)]
mod console {
    use std::io;

    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, GetStdHandle, SetConsoleCP,
        SetConsoleMode, SetConsoleOutputCP, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };

    #[derive(Clone, Copy)]
    pub(super) struct ConsoleState {
        input_cp: u32,
        output_cp: u32,
        output_mode: u32,
    }

    impl ConsoleState {
        fn new(input_cp: u32, output_cp: u32, output_mode: u32) -> Self {
            Self {
                input_cp,
                output_cp,
                output_mode,
            }
        }

        #[cfg(test)]
        fn code_pages(self) -> (u32, u32) {
            (self.input_cp, self.output_cp)
        }
    }

    /// Spec §22.1 step 2: activate UTF-8 + VT output before raw mode.
    pub fn setup() -> io::Result<ConsoleState> {
        // Safety: plain FFI calls on the process console; no invariants.
        let input_cp = unsafe { GetConsoleCP() };
        let output_cp = unsafe { GetConsoleOutputCP() };
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
        Ok(ConsoleState::new(input_cp, output_cp, prev_mode))
    }

    pub fn restore(state: ConsoleState) {
        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        unsafe {
            if state.output_mode != u32::MAX {
                SetConsoleMode(handle, state.output_mode);
            }
            if state.output_cp != 0 {
                SetConsoleOutputCP(state.output_cp);
            }
            if state.input_cp != 0 {
                SetConsoleCP(state.input_cp);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::ConsoleState;

        #[test]
        fn preserves_distinct_input_and_output_code_pages() {
            let state = ConsoleState::new(437, 1252, 7);

            assert_eq!(state.code_pages(), (437, 1252));
        }
    }
}

#[cfg(windows)]
static ACTIVE_CONSOLE: std::sync::Mutex<Option<console::ConsoleState>> =
    std::sync::Mutex::new(None);

/// Installs a panic hook that restores terminal state before delegating to the
/// previously installed hook, ensuring errors are readable on a clean terminal
/// buffer and raw mode is disabled even on panic (tui-design ecosystem-rust).
pub fn install_panic_hook() {
    static HOOK_INSTALLED: std::sync::Once = std::sync::Once::new();
    HOOK_INSTALLED.call_once(|| {
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let mut stdout = io::stdout();
            let _ = restore_surface(&mut stdout, true);
            let _ = disable_raw_mode();
            #[cfg(windows)]
            if let Ok(mut guard) = ACTIVE_CONSOLE.lock() {
                if let Some(console) = guard.take() {
                    console::restore(console);
                }
            }
            prev_hook(panic_info);
        }));
    });
}

impl TerminalGuard {
    pub fn enter(stdout: &mut Stdout) -> io::Result<Self> {
        #[cfg(windows)]
        let console = console::setup()?;
        #[cfg(windows)]
        if let Ok(mut guard) = ACTIVE_CONSOLE.lock() {
            *guard = Some(console);
        }
        install_panic_hook();
        #[cfg(windows)]
        enable_with_rollback(enable_raw_mode, || {
            if let Ok(mut guard) = ACTIVE_CONSOLE.lock() {
                guard.take();
            }
            console::restore(console);
        })?;
        #[cfg(not(windows))]
        enable_raw_mode()?;
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            #[cfg(windows)]
            {
                if let Ok(mut guard) = ACTIVE_CONSOLE.lock() {
                    guard.take();
                }
                console::restore(console);
            }
            return Err(error);
        }
        Ok(Self {
            active: true,
            mouse_capture: false,
            #[cfg(windows)]
            console: Some(console),
        })
    }

    pub fn enable_mouse_capture<W: Write>(&mut self, stdout: &mut W) -> io::Result<()> {
        execute!(stdout, EnableMouseCapture)?;
        self.mouse_capture = true;
        Ok(())
    }

    pub fn restore<W: Write>(&mut self, stdout: &mut W) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        #[cfg(windows)]
        if let Ok(mut guard) = ACTIVE_CONSOLE.lock() {
            guard.take();
        }
        let leave_result = restore_surface(stdout, self.mouse_capture);
        let raw_result = disable_raw_mode();
        #[cfg(windows)]
        if let Some(console) = self.console {
            console::restore(console);
        }
        leave_result.and(raw_result)
    }
}

fn enable_with_rollback(
    enable: impl FnOnce() -> io::Result<()>,
    rollback: impl FnOnce(),
) -> io::Result<()> {
    enable().inspect_err(|_| rollback())
}

fn restore_surface<W: Write>(stdout: &mut W, mouse_capture: bool) -> io::Result<()> {
    execute!(stdout, Show, LeaveAlternateScreen, DisableBracketedPaste)?;
    if mouse_capture {
        execute!(stdout, DisableMouseCapture)?;
    }
    Ok(())
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            #[cfg(windows)]
            if let Ok(mut guard) = ACTIVE_CONSOLE.lock() {
                guard.take();
            }
            let mut stdout = io::stdout();
            let _ = restore_surface(&mut stdout, self.mouse_capture);
            let _ = disable_raw_mode();
            #[cfg(windows)]
            if let Some(console) = self.console {
                console::restore(console);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;

    use super::enable_with_rollback;

    #[test]
    fn raw_mode_failure_runs_console_rollback() {
        let rolled_back = Cell::new(false);

        let result = enable_with_rollback(
            || Err(io::Error::other("raw mode failed")),
            || rolled_back.set(true),
        );

        assert!(result.is_err());
        assert!(rolled_back.get());
    }

    #[test]
    fn restore_surface_skips_mouse_cleanup_when_capture_was_never_enabled() {
        let mut output = Vec::new();

        super::restore_surface(&mut output, false)
            .expect("terminal cleanup without mouse capture should succeed");
    }

    #[test]
    fn install_panic_hook_is_idempotent() {
        super::install_panic_hook();
        super::install_panic_hook();
    }
}
