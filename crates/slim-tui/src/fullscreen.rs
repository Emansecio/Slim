use std::io::{self, BufWriter, Stdout};

use crossterm::cursor::{Hide, SetCursorStyle};
use crossterm::event::{EnableBracketedPaste, EnableFocusChange};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{execute, queue};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::terminal::TerminalGuard;
use crate::theme::{env_flag_enabled, Capabilities};

/// A frame of a large styled transcript is tens of kilobytes of escape
/// sequences. `Stdout` itself is line-buffered in 1 KiB, so each frame would
/// be dozens of `write` calls under its lock; this buffer makes it about one.
const FRAME_BUFFER_BYTES: usize = 128 * 1024;

type FrameWriter = BufWriter<Stdout>;

pub struct FullscreenBackend {
    terminal: Terminal<CrosstermBackend<FrameWriter>>,
    guard: TerminalGuard,
    /// DEC 2026 synchronized output; `SLIM_SYNC_OUTPUT=0` turns it off.
    synchronized: bool,
}

impl FullscreenBackend {
    /// Enter order per spec §22.1: raw mode → alternate screen → bracketed
    /// paste → mouse (capability-gated, §18 "opcional e desligável") → cursor
    /// hidden until the first focused frame.
    pub fn start(capabilities: Capabilities) -> io::Result<Self> {
        let mut stdout = io::stdout();
        let mut guard = TerminalGuard::enter(&mut stdout)?;
        // Focus events let motion pause while the window is in the background;
        // terminals without them simply never send one.
        if let Err(error) = execute!(stdout, EnableBracketedPaste, EnableFocusChange) {
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
        let writer = BufWriter::with_capacity(FRAME_BUFFER_BYTES, stdout);
        let terminal = Terminal::new(CrosstermBackend::new(writer))?;
        Ok(Self {
            terminal,
            guard,
            synchronized: env_flag_enabled("SLIM_SYNC_OUTPUT", true),
        })
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<FrameWriter>> {
        &mut self.terminal
    }

    /// Draws one frame inside a synchronized update, so the terminal
    /// presents it whole instead of mid-write. Terminals without DEC 2026
    /// ignore the markers. The update is closed even if drawing fails.
    pub fn draw_synchronized<F>(&mut self, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        if self.synchronized {
            queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        }
        let drawn = self.terminal.draw(render).map(|_| ());
        if self.synchronized {
            let ended = execute!(self.terminal.backend_mut(), EndSynchronizedUpdate);
            return drawn.and(ended);
        }
        drawn
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        let backend = self.terminal.backend_mut();
        self.guard.restore(backend)?;
        Ok(())
    }
}
