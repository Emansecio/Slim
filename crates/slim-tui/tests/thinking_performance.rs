//! Local deterministic presentation benchmark; no provider or real terminal.
use std::time::Instant;

use ratatui::{backend::TestBackend, Terminal};
use slim_tui::app::AppState;
use slim_tui::block::{Block, BlockKind, BlockLifecycle, FoldState};
use slim_tui::render::{HeightIndex, WrapCache};
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

fn corpus(groups: usize, members: usize) -> AppState {
    let mut state = AppState::new();
    state.authenticated = true;
    for group in 0..groups {
        for member in 0..members {
            let mut block = Block::new(
                format!("thought-{group}-{member}"),
                BlockKind::Thinking("análise do arquivo: ação, 日本語 e 👩‍💻\n".repeat(100)),
                BlockLifecycle::Complete,
            );
            block.fold = FoldState::Expanded;
            assert!(state.append_block(block));
        }
        assert!(state.append_block(Block::new(
            format!("answer-{group}"),
            BlockKind::Assistant("Resultado verificado.".into()),
            BlockLifecycle::Complete,
        )));
    }
    state
}

fn report(name: &str, mut samples: Vec<f64>) {
    samples.sort_by(f64::total_cmp);
    eprintln!(
        "{name}: n={} median_ms={:.3} min_ms={:.3} max_ms={:.3}",
        samples.len(),
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1]
    );
}

#[test]
#[ignore = "manual local performance measurement"]
fn expanded_thinking_frames() {
    for (name, groups, members) in [("history", 40, 4), ("visible_group", 1, 160)] {
        let state = corpus(groups, members);
        let capabilities = Capabilities {
            color_depth: ColorDepth::TrueColor,
            mouse: false,
            clipboard: false,
            images: false,
            reduced_motion: true,
        };
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        let mut cache = WrapCache::default();
        let start = Instant::now();
        terminal
            .draw(|frame| render_frame(frame, &state, capabilities, &mut cache))
            .unwrap();
        eprintln!(
            "{name}: first_frame_ms={:.3}",
            start.elapsed().as_secs_f64() * 1000.0
        );
        let expected = terminal.backend().buffer().clone();
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        format!("{expected:?}").hash(&mut hasher);
        eprintln!("{name}: frame_hash={:016x}", hasher.finish());
        let mut frames = Vec::new();
        let mut indices = Vec::new();
        for _ in 0..11 {
            let start = Instant::now();
            terminal
                .draw(|frame| render_frame(frame, &state, capabilities, &mut cache))
                .unwrap();
            frames.push(start.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(terminal.backend().buffer(), &expected);
            let start = Instant::now();
            std::hint::black_box(HeightIndex::build(state.blocks(), 139, &mut cache));
            indices.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        report(&format!("{name}_frames"), frames);
        report(&format!("{name}_index"), indices);
    }
}

/// Cost of one streamed delta (reduce + draw) as the message being written
/// grows. Reports the median and the worst of 40 deltas at each size, for the
/// answer text and for the reasoning preview.
#[test]
#[ignore = "manual release measurement of streaming cost"]
fn streaming_delta_cost_by_size() {
    use slim_tui::api::UiEvent;
    let capabilities = Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    };
    let delta = "alguma coisa sobre o cache e seus tamanhos ";
    for name in ["assistant", "thinking"] {
        let event = |text: String| match name {
            "assistant" => UiEvent::AssistantDelta { text },
            _ => UiEvent::ThinkingDelta { text },
        };
        let mut state = AppState::new();
        state.authenticated = true;
        state.apply_event(UiEvent::UserMessageAdded {
            text: "pergunta".into(),
        });
        state.apply_event(UiEvent::run_started(1));
        if name == "thinking" {
            state.apply_event(UiEvent::ThinkingStarted);
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let mut cache = WrapCache::default();
        let mut written = 0usize;
        for checkpoint in [2_000usize, 20_000, 100_000, 400_000] {
            // Reach the size without drawing, as buffered deltas would.
            while written + 4_096 < checkpoint {
                let chunk = delta.repeat(4_096 / delta.len());
                written += chunk.len();
                state.apply_event(event(chunk));
            }
            let mut samples = Vec::new();
            for tick in 0..40u64 {
                state.clock.frame = tick;
                state.clock.elapsed_ms = tick * 83;
                let start = Instant::now();
                written += delta.len();
                state.apply_event(event(delta.to_owned()));
                terminal
                    .draw(|frame| render_frame(frame, &state, capabilities, &mut cache))
                    .unwrap();
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            report(&format!("{name} at ~{written} bytes"), samples);
        }
    }
}

/// Bytes the terminal receives per frame: a delta that stays on the last row
/// against one that pushes a new row and scrolls the whole transcript.
#[test]
#[ignore = "manual measurement of terminal output volume"]
fn streaming_output_bytes_per_frame() {
    use ratatui::backend::CrosstermBackend;
    use slim_tui::api::UiEvent;
    let capabilities = Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    };
    for (width, height) in [(120u16, 40u16), (200, 60)] {
        let mut state = AppState::new();
        state.authenticated = true;
        state.apply_event(UiEvent::UserMessageAdded {
            text: "pergunta".into(),
        });
        state.apply_event(UiEvent::run_started(1));
        // Rows that differ almost everywhere, as real prose does: repeated or
        // near-identical rows would make the scroll look free, because the
        // diff would find few changed cells.
        const WORDS: [&str; 24] = [
            "cache",
            "parser",
            "tokens",
            "função",
            "arquivo",
            "erro",
            "teste",
            "módulo",
            "leitura",
            "escrita",
            "camada",
            "fluxo",
            "estado",
            "evento",
            "buffer",
            "linha",
            "resultado",
            "projeto",
            "contexto",
            "ajuste",
            "medida",
            "limite",
            "versão",
            "caminho",
        ];
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut word = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            WORDS[(seed >> 33) as usize % WORDS.len()]
        };
        let text: String = (0..120)
            .map(|_| {
                let line: Vec<&str> = (0..12).map(|_| word()).collect();
                format!("{}\n\n", line.join(" "))
            })
            .collect();
        state.apply_event(UiEvent::AssistantDelta { text });
        // Counts what the terminal would receive without keeping it.
        #[derive(Clone, Default)]
        struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl std::io::Write for Counter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .fetch_add(buf.len(), std::sync::atomic::Ordering::Relaxed);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let counter = Counter::default();
        // A fixed viewport: a plain terminal would ask the real console for
        // its size and ignore the one under test.
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(counter.clone()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, width, height)),
            },
        )
        .expect("terminal");
        let mut cache = WrapCache::default();
        let mut draw = |state: &AppState, terminal: &mut Terminal<CrosstermBackend<Counter>>| {
            terminal
                .draw(|frame| render_frame(frame, state, capabilities, &mut cache))
                .unwrap();
            counter.0.swap(0, std::sync::atomic::Ordering::Relaxed)
        };
        let first = draw(&state, &mut terminal);
        let mut same_row = Vec::new();
        let mut new_row = Vec::new();
        for step in 0..20u64 {
            state.clock.frame = step;
            state.apply_event(UiEvent::AssistantDelta {
                text: "mais".into(),
            });
            same_row.push(draw(&state, &mut terminal) as f64);
            state.apply_event(UiEvent::AssistantDelta {
                text: "\n\nnova linha de resposta que empurra a tela para cima".into(),
            });
            new_row.push(draw(&state, &mut terminal) as f64);
        }
        eprintln!("{width}x{height}: first_frame_bytes={first}");
        for (name, mut bytes) in [("same_row", same_row), ("new_row", new_row)] {
            bytes.sort_by(f64::total_cmp);
            eprintln!(
                "{width}x{height}: {name}_bytes median={} min={} max={}",
                bytes[bytes.len() / 2],
                bytes[0],
                bytes[bytes.len() - 1]
            );
        }
    }
}

#[test]
#[ignore = "manual release measurement of a large visible member"]
fn oversized_thinking_costs() {
    use slim_tui::api::UiEvent;
    use slim_tui::app::FollowMode;
    let capabilities = Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: true,
    };
    for repeats in [1024, 16384] {
        let text = "análise do arquivo: ação, 日本語 e 👩‍💻\n".repeat(repeats);
        for streaming in [false, true] {
            let mut state = AppState::new();
            state.authenticated = true;
            if streaming {
                state.apply_event(UiEvent::run_started(1));
                state.apply_event(UiEvent::ThinkingStarted);
                state.apply_event(UiEvent::ThinkingDelta { text: text.clone() });
                let id = state.blocks().last().unwrap().id.clone();
                assert!(state.toggle_block(&id));
            } else {
                for (id, content) in [
                    ("leader", "short thought".to_string()),
                    ("large", text.clone()),
                ] {
                    let mut block =
                        Block::new(id, BlockKind::Thinking(content), BlockLifecycle::Complete);
                    block.fold = FoldState::Expanded;
                    assert!(state.append_block(block));
                }
            }
            let mut cache = WrapCache::default();
            let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
            terminal
                .draw(|f| render_frame(f, &state, capabilities, &mut cache))
                .unwrap();
            eprintln!("visible_bytes={} streaming={streaming}", text.len());
            for action in ["warm", "scroll", "resize", "delta"] {
                if action == "delta" && !streaming {
                    continue;
                }
                let mut samples = Vec::new();
                for i in 0..11 {
                    let start = Instant::now();
                    match action {
                        "scroll" => {
                            state.scroll.mode = if i % 2 == 0 {
                                FollowMode::Top
                            } else {
                                FollowMode::default()
                            }
                        }
                        "resize" => terminal
                            .resize(ratatui::layout::Rect::new(
                                0,
                                0,
                                if i % 2 == 0 { 100 } else { 140 },
                                40,
                            ))
                            .unwrap(),
                        "delta" => state.apply_event(UiEvent::ThinkingDelta {
                            text: "more\n".into(),
                        }),
                        _ => {}
                    }
                    terminal
                        .draw(|f| render_frame(f, &state, capabilities, &mut cache))
                        .unwrap();
                    samples.push(start.elapsed().as_secs_f64() * 1000.0);
                }
                report(action, samples);
            }
        }
    }
}
