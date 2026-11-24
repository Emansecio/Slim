use std::io::{self, BufWriter, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossterm::cursor::{Hide, SetCursorStyle};
use crossterm::event::{EnableBracketedPaste, EnableFocusChange, EnableMouseCapture};
use crossterm::queue;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::Terminal;

use crate::terminal::TerminalGuard;
use crate::theme::{env_flag_enabled, Capabilities};

/// A frame of a large styled transcript is tens of kilobytes of escape
/// sequences. `Stdout` itself is line-buffered in 1 KiB, so each frame would
/// be dozens of `write` calls under its lock; this buffer makes it about one.
const FRAME_BUFFER_BYTES: usize = 128 * 1024;

/// Buffered frame writer that defers `Terminal::draw`'s internal `flush()`
/// while DEC 2026 synchronized output is active so `BeginSynchronizedUpdate`,
/// the frame diff, and `EndSynchronizedUpdate` leave in one atomic `flush_now()`.
pub struct SyncFrameWriter<W: Write = Stdout> {
    inner: BufWriter<W>,
    defer_flush: Arc<AtomicBool>,
}

impl<W: Write> SyncFrameWriter<W> {
    fn new(inner: BufWriter<W>) -> Self {
        Self {
            inner,
            defer_flush: Arc::new(AtomicBool::new(false)),
        }
    }

    fn defer_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.defer_flush)
    }
}

impl<W: Write> Write for SyncFrameWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.inner.write_vectored(bufs)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.defer_flush.load(Ordering::Relaxed) {
            Ok(())
        } else {
            self.inner.flush()
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.inner.write_all(buf)
    }
}

pub type FrameWriter = SyncFrameWriter<Stdout>;

pub struct FullscreenBackend {
    terminal: Terminal<CrosstermBackend<FrameWriter>>,
    guard: TerminalGuard,
    defer_flush: Arc<AtomicBool>,
    /// DEC 2026 synchronized output; `SLIM_SYNC_OUTPUT=0` turns it off.
    synchronized: bool,
}

fn queue_entry_sequence<W: Write>(
    writer: &mut W,
    mouse: bool,
    on_mouse_queued: impl FnOnce(),
) -> io::Result<()> {
    queue!(
        writer,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    if mouse {
        queue!(writer, EnableMouseCapture)?;
        on_mouse_queued();
    }
    queue!(writer, SetCursorStyle::BlinkingBar, Hide)?;
    writer.flush()
}

impl FullscreenBackend {
    /// Enter order per spec §22.1: raw mode → buffered alternate screen →
    /// bracketed paste → focus events → mouse (capability-gated, §18 "opcional
    /// e desligável") → cursor style + hidden until the first focused frame,
    /// issued in one atomic `flush()`.
    pub fn start(capabilities: Capabilities) -> io::Result<Self> {
        let mut guard = TerminalGuard::enter_raw()?;
        let mut writer = BufWriter::with_capacity(FRAME_BUFFER_BYTES, io::stdout());
        let mut mouse_enabled = false;
        let entry = queue_entry_sequence(&mut writer, capabilities.mouse, || {
            mouse_enabled = true;
        });
        if mouse_enabled {
            // Ensure rollback disables mouse capture if queued or flush fails.
            let _ = guard.enable_mouse_capture_queued(&mut io::sink());
        }
        if let Err(error) = entry {
            drop(writer);
            drop(guard);
            return Err(error);
        }
        let frame_writer = SyncFrameWriter::new(writer);
        let defer_flush = frame_writer.defer_flag();
        let terminal = match Terminal::new(CrosstermBackend::new(frame_writer)) {
            Ok(terminal) => terminal,
            Err(error) => {
                drop(guard);
                return Err(error);
            }
        };
        Ok(Self {
            terminal,
            guard,
            defer_flush,
            synchronized: env_flag_enabled("SLIM_SYNC_OUTPUT", true),
        })
    }

    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<FrameWriter>> {
        &mut self.terminal
    }

    /// Draws one frame inside a synchronized update, so the terminal
    /// presents it whole instead of mid-write. Terminals without DEC 2026
    /// ignore the markers. The update is closed and flushed even if drawing
    /// fails.
    pub fn draw_synchronized<F>(&mut self, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        if self.synchronized {
            self.defer_flush.store(true, Ordering::Relaxed);
            if let Err(error) = queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate) {
                self.defer_flush.store(false, Ordering::Relaxed);
                let _ = Backend::flush(self.terminal.backend_mut());
                return Err(error);
            }
        }
        let drawn = self.terminal.draw(render).map(|_| ());
        if self.synchronized {
            let ended = queue!(self.terminal.backend_mut(), EndSynchronizedUpdate);
            self.defer_flush.store(false, Ordering::Relaxed);
            let flushed = Backend::flush(self.terminal.backend_mut());
            return drawn.and(ended).and(flushed);
        }
        drawn
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        let backend = self.terminal.backend_mut();
        self.guard.restore(backend)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct MockWriter {
        bytes: Rc<std::cell::RefCell<Vec<u8>>>,
        flushes: Rc<Cell<usize>>,
        fail_flush: bool,
    }

    impl MockWriter {
        fn new(fail_flush: bool) -> Self {
            Self {
                bytes: Rc::new(std::cell::RefCell::new(Vec::new())),
                flushes: Rc::new(Cell::new(0)),
                fail_flush,
            }
        }
    }

    impl Write for MockWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes.set(self.flushes.get() + 1);
            if self.fail_flush {
                Err(io::Error::other("flush failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn buffered_entry_writes_exact_order_and_flushes_once() {
        let raw = MockWriter::new(false);
        let bytes = Rc::clone(&raw.bytes);
        let flushes = Rc::clone(&raw.flushes);
        let mut buffered = BufWriter::with_capacity(FRAME_BUFFER_BYTES, raw);
        let mut mouse_marked = false;
        queue_entry_sequence(&mut buffered, true, || mouse_marked = true).expect("entry");
        assert!(mouse_marked);
        assert_eq!(flushes.get(), 1);
        let written = String::from_utf8_lossy(&bytes.borrow()).into_owned();
        let alt = written.find("\x1b[?1049h").expect("alt screen");
        let paste = written.find("\x1b[?2004h").expect("bracketed paste");
        let focus = written.find("\x1b[?1004h").expect("focus");
        let mouse = written.find("\x1b[?1000h").expect("mouse");
        let cursor_style = written.find("\x1b[5 q").expect("blinking bar");
        let hide = written.find("\x1b[?25l").expect("hide cursor");
        assert!(alt < paste && paste < focus && focus < mouse && mouse < cursor_style && cursor_style < hide);
    }

    #[test]
    fn sync_frame_writer_defers_intermediate_flushes_until_flush_now() {
        let raw = MockWriter::new(false);
        let bytes = Rc::clone(&raw.bytes);
        let flushes = Rc::clone(&raw.flushes);
        let mut writer = SyncFrameWriter::new(BufWriter::with_capacity(FRAME_BUFFER_BYTES, raw));
        let defer_flush = writer.defer_flag();

        defer_flush.store(true, Ordering::Relaxed);
        queue!(&mut writer, BeginSynchronizedUpdate).unwrap();
        writer.write_all(b"frame-body").unwrap();
        writer.flush().unwrap();
        assert_eq!(flushes.get(), 0);
        assert!(bytes.borrow().is_empty());

        queue!(&mut writer, EndSynchronizedUpdate).unwrap();
        defer_flush.store(false, Ordering::Relaxed);
        writer.flush().unwrap();
        assert_eq!(flushes.get(), 1);
        let out = bytes.borrow();
        assert!(out.starts_with(b"\x1b[?2026hframe-body\x1b[?2026l"));
    }
}
