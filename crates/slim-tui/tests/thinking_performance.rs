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
