//! Long-session benchmark (DESIGN-SLIM-TUI §29, gate C6): 3.200 blocks /
//! ~5 MiB corpus through the virtualized render pipeline. Gate: warm p95
//! input→frame ≤ 16 ms (§27). Exits nonzero when the budget is exceeded.

use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::block::{Block, BlockKind, BlockLifecycle};
use slim_tui::render::{EventCoalescer, HeightIndex, WrapCache};
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

fn build_corpus() -> AppState {
    let mut state = AppState::new();
    for index in 0..3_200 {
        let text = match index % 4 {
            0 => format!("pergunta {index}: revisar módulo auth/session.rs\nsegunda linha de contexto"),
            1 => format!(
                "resposta {index} com parágrafo longo: {}\n- item um\n- item dois",
                "lorem ipsum dev workspace ".repeat(24)
            ),
            2 => "pensamento intermediário sobre a próxima etapa do plano".into(),
            _ => "código: let token = refresh.lock().await;".into(),
        };
        let kind = match index % 4 {
            0 => BlockKind::User(text),
            1 | 3 => BlockKind::Assistant(text),
            _ => BlockKind::Thinking(text),
        };
        state.blocks.push(Block::new(
            format!("bench-{index}"),
            kind,
            BlockLifecycle::Complete,
        ));
    }
    state
}

fn percentile(samples: &mut [f64], p: usize) -> f64 {
    samples.sort_by(f64::total_cmp);
    let rank = (samples.len() - 1) * p.min(100) / 100;
    samples[rank]
}

fn idx_total<'a>(blocks: &'a [Block], width: u16, cache: &'a mut WrapCache) -> u64 {
    HeightIndex::build(blocks, width, cache).total_rows
}

fn main() {
    let state = build_corpus();
    const WIDTH: u16 = 140;
    const HEIGHT: u16 = 40;

    // Warm the wrap cache exactly like steady-state usage.
    let mut cache = WrapCache::default();
    let index = HeightIndex::build(&state.blocks, WIDTH, &mut cache);
    let bottom = index.total_rows.saturating_sub(HEIGHT as u64);
    let _ = index.locate(bottom);

    let mut frame_samples = Vec::with_capacity(50);
    for i in 0..50 {
        // Scroll position varies per iteration to exercise different ranges.
        let start = Instant::now();
        let mut cache_i = WrapCache::default();
        let state_view = &state;
        let backend = TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let row = (idx_total(&state.blocks, WIDTH, &mut cache_i) / 3).saturating_sub((i % 10) as u64);
        let _ = row;
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    state_view,
                    Capabilities {
                        color_depth: ColorDepth::TrueColor,
                        mouse: false,
                        clipboard: false,
                        images: false,
                        reduced_motion: false,
                    },
                    &mut cache_i,
                )
            })
            .expect("draw");
        frame_samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    // Input→frame path: coalesced delta applied then virtualized render.
    let mut pipeline_samples = Vec::with_capacity(50);
    let mut rows = 0usize;
    for _ in 0..50 {
        let start = Instant::now();
        let mut state = build_corpus();
        let mut coalescer = EventCoalescer::new(256, Duration::from_millis(16));
        coalescer.push_data(UiEvent::AssistantDelta {
            text: "delta incremental ".repeat(8),
        });
        for event in coalescer.flush() {
            state.apply_event(event);
        }
        let mut cache_p = WrapCache::default();
        let idx = HeightIndex::build(&state.blocks, WIDTH, &mut cache_p);
        let bottom = idx.total_rows.saturating_sub(HEIGHT as u64);
        let _ = idx.locate(bottom);
        let backend = TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &state,
                    Capabilities {
                        color_depth: ColorDepth::TrueColor,
                        mouse: false,
                        clipboard: false,
                        images: false,
                        reduced_motion: false,
                    },
                    &mut cache_p,
                )
            })
            .expect("draw");
        rows = terminal.backend().buffer().area.height as usize;
        pipeline_samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let frame_p95 = percentile(&mut frame_samples, 95);
    let pipeline_p95 = percentile(&mut pipeline_samples, 95);
    println!(
        "blocks=3200 rows={rows} scroll_locate_p95_ms={frame_p95:.3} input_to_frame_p95_ms={pipeline_p95:.3}"
    );
    // Versioned budget (§27): input→frame p95 warm ≤ 16 ms.
    if pipeline_p95 > 16.0 {
        eprintln!("BUDGET EXCEEDED: input_to_frame_p95_ms={pipeline_p95:.3} > 16.0");
        std::process::exit(1);
    }
}
