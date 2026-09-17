use std::io::{self, Stdout};

use crossterm::cursor::{Hide, SetCursorStyle};
use crossterm::event::EnableBracketedPaste;
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::terminal::TerminalGuard;
use crate::theme::Capabilities;

pub struct FullscreenBackend {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    guard: TerminalGuard,
}

impl FullscreenBackend {
    /// Enter order per spec §22.1: raw mode → alternate screen → bracketed
    /// paste → mouse (capability-gated, §18 "opcional e desligável") → cursor
    /// hidden until the first focused frame.
    pub fn start(capabilities: Capabilities) -> io::Result<Self> {
        let mut stdout = io::stdout();
        let mut guard = TerminalGuard::enter(&mut stdout)?;
        if let Err(error) = execute!(stdout, EnableBracketedPaste) {
            drop(guard);
            return Err(error);
        }
        if capabilities.mouse {
            if let Err(error) = guard.enable_mouse_capture(&mut stdout) {
                drop(guard);
                return Err(error);
            }
        }
        let _ = execute!(stdout, SetCursorStyle::BlinkingBar, Hide);
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal, guard })
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        let backend = self.terminal.backend_mut();
        self.guard.restore(backend)?;
        Ok(())
    }
}
