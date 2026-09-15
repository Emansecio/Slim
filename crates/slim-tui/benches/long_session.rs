//! Long-session benchmark (DESIGN-SLIM-TUI §29, gate C6): 3.200 blocks /
//! ~5 MiB corpus through the virtualized render pipeline. Gate: warm p95
//! input→frame ≤ 16 ms (§27). Exits nonzero when the budget is exceeded.

use std::time::{Duration, Instant};

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::block::{Block, BlockKind, BlockLifecycle, FoldState, ToolState};
use slim_tui::render::{EventCoalescer, HeightIndex, WrapCache};
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

fn build_corpus() -> AppState {
    let mut state = AppState::new();
    for index in 0..3_200 {
        let kind = if index == 3_199 {
            BlockKind::Assistant("live streaming tail".into())
        } else {
            match index % 8 {
                0 => BlockKind::User(format!(
                    "pergunta {index}: {}",
                    "contexto detalhado do workspace ".repeat(80)
                )),
                1 => BlockKind::Assistant(format!(
                    "## Resposta {index}\n{}\n- item um\n- item dois",
                    "lorem ipsum dev workspace ".repeat(80)
                )),
                2 => BlockKind::Thinking("análise intermediária da próxima etapa ".repeat(56)),
                3 => BlockKind::Assistant(format!(
                    "## Patch {index}\n```diff\n{}\n```",
                    "+ let token = refresh.lock().await;\n".repeat(36)
                )),
                4 => BlockKind::Tool(ToolState {
                    historical: false,
                    batch_id: slim_tui::api::ToolBatchId(format!("bench-batch-{index}").into()),
                    call_id: slim_tui::api::ToolCallId(format!("bench-call-{index}").into()),
                    name: format!("read-{index}"),
                    arguments_summary: String::new(),
                    preview: "output line\n".repeat(96),
                    duration_ms: None,
                    content_handle: None,
                    materialized_output: String::new(),
                    next_cursor: None,
                    pending_page: None,
                }),
                5 => BlockKind::Error("stack frame: provider request failed\n".repeat(40)),
                6 => BlockKind::System("system checkpoint complete\n".repeat(24)),
                _ => BlockKind::QueuedUser("queued workspace prompt\n".repeat(32)),
            }
        };
        let mut block = Block::new(format!("bench-{index}"), kind, BlockLifecycle::Complete);
        if matches!(block.kind(), BlockKind::Thinking(_)) {
            block.fold = if (index / 8) % 2 == 0 {
                FoldState::Collapsed
            } else {
                FoldState::Expanded
            };
        }
        assert!(state.append_block(block));
    }
    state
}

fn corpus_bytes(state: &AppState) -> usize {
    state
        .blocks()
        .iter()
        .map(|block| match block.kind() {
            BlockKind::User(text)
            | BlockKind::Assistant(text)
            | BlockKind::Thinking(text)
            | BlockKind::System(text)
            | BlockKind::Error(text)
            | BlockKind::Activity(text)
            | BlockKind::QueuedUser(text) => text.len(),
            BlockKind::Tool(tool) => tool.name.len() + tool.preview.len(),
            BlockKind::InteractionRequest(interaction) => interaction.display_text().len(),
        })
        .sum()
}

fn percentile(samples: &mut [f64], p: usize) -> f64 {
    samples.sort_by(f64::total_cmp);
    let rank = if p == 0 {
        0
    } else {
        samples
            .len()
            .saturating_mul(p.min(100))
            .div_ceil(100)
            .saturating_sub(1)
    };
    samples[rank.min(samples.len().saturating_sub(1))]
}

fn main() {
    let mut percentile_fixture = (0..50).map(f64::from).collect::<Vec<_>>();
    assert_eq!(percentile(&mut percentile_fixture, 95), 47.0);

    let mut state = build_corpus();
    let bytes = corpus_bytes(&state);
    assert!(
        (4_500_000..=5_500_000).contains(&bytes),
        "long-session corpus must stay near 5 MiB, got {bytes} bytes"
    );
    assert!(state
        .blocks()
        .iter()
        .any(|block| matches!(block.kind(), BlockKind::Tool(_))));
    assert!(state
        .blocks()
        .iter()
        .any(|block| matches!(block.kind(), BlockKind::Error(_))));
    assert!(state
        .blocks()
        .iter()
        .any(|block| block.fold == slim_tui::block::FoldState::Expanded));
    const WIDTH: u16 = 140;
    const HEIGHT: u16 = 40;

    // Warm the wrap cache exactly like steady-state usage.
    let mut cache = WrapCache::default();
    let index = HeightIndex::build(state.blocks(), WIDTH, &mut cache);
    let bottom = index.total_rows.saturating_sub(HEIGHT as u64);
    let _ = index.locate(bottom);

    let capabilities = Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    };
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend).expect("terminal");

    // Warm steady-state frame: the terminal and wrap cache persist exactly as
    // they do in run_loop; HeightIndex still scans all 3,200 cached entries.
    let mut frame_samples = Vec::with_capacity(50);
    for _ in 0..50 {
        let start = Instant::now();
        terminal
            .draw(|frame| render_frame(frame, &state, capabilities, &mut cache))
            .expect("draw");
        frame_samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let body_total = cache.body_hits() + cache.body_misses();
    eprintln!(
        "SLIM_BODY_CACHE hits={} misses={} hit_rate={:.2}% evictions={} retained_bytes={} bypasses={}",
        cache.body_hits(),
        cache.body_misses(),
        if body_total > 0 {
            cache.body_hits() as f64 * 100.0 / body_total as f64
        } else {
            0.0
        },
        cache.body_evictions(),
        cache.body_retained_bytes(),
        cache.body_bypasses(),
    );

    // Input→frame steady state: each coalesced delta advances one block's
    // generation, forcing exactly that height to be recomputed.
    let mut pipeline_samples = Vec::with_capacity(50);
    let mut coalescer = EventCoalescer::new(256, Duration::from_millis(16));
    for _ in 0..50 {
        let start = Instant::now();
        for event in coalescer.push_data(UiEvent::AssistantDelta {
            text: "delta incremental ".repeat(8),
        }) {
            state.apply_event(event);
        }
        for event in coalescer.flush() {
            state.apply_event(event);
        }
        terminal
            .draw(|frame| render_frame(frame, &state, capabilities, &mut cache))
            .expect("draw");
        pipeline_samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let rows = terminal.backend().buffer().area.height as usize;

    let frame_p95 = percentile(&mut frame_samples, 95);
    let pipeline_p95 = percentile(&mut pipeline_samples, 95);
    println!(
        "blocks=3200 bytes={bytes} rows={rows} scroll_locate_p95_ms={frame_p95:.3} input_to_frame_p95_ms={pipeline_p95:.3}"
    );
    // Versioned budget (§27): input→frame p95 warm ≤ 16 ms.
    if pipeline_p95 > 16.0 {
        eprintln!("BUDGET EXCEEDED: input_to_frame_p95_ms={pipeline_p95:.3} > 16.0");
        std::process::exit(1);
    }
}
