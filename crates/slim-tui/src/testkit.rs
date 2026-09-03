use crate::view_model::Frame;

/// Deterministic full-screen terminal materialization for cross-crate golden
/// tests. Production callers continue to use `run_app`/`render_frame`.
pub fn render_terminal_text(state: &crate::app::AppState, width: u16, height: u16) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut cache = crate::render::WrapCache::default();
    terminal
        .draw(|frame| {
            crate::runtime::render_frame(
                frame,
                state,
                crate::theme::Capabilities {
                    color_depth: crate::theme::ColorDepth::TrueColor,
                    mouse: false,
                    clipboard: false,
                    images: false,
                    reduced_motion: false,
                },
                &mut cache,
            )
        })
        .expect("test draw");
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            output.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        output.push('\n');
    }
    output
}

#[derive(Default)]
pub struct MemorySurface {
    pub frames: Vec<Frame>,
}

impl MemorySurface {
    pub fn draw(&mut self, frame: Frame) {
        self.frames.push(frame);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FakeClock {
    pub ticks: u64,
}
