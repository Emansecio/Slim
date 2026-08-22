use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{poll, read, Event, KeyCode, KeyEventKind};
use slim_tui::fullscreen::FullscreenBackend;
use slim_tui::composer::Composer;
use slim_tui::app::AppState;
use slim_tui::block::{Block, BlockKind, BlockLifecycle};
use slim_tui::view_model::ViewModel;
use slim_tui::theme::detect_capabilities;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--capability-probe") {
        let capabilities = detect_capabilities();
        println!("capabilities={capabilities:?}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|arg| arg == "--composer-probe") {
        return composer_probe();
    }
    if args.iter().any(|arg| arg == "--event-debug") {
        return event_debug();
    }
    if args.iter().any(|arg| arg == "--long-session-probe") {
        return long_session_probe();
    }
    let cycles = args
        .iter()
        .position(|arg| arg == "--cycles")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(1);
    let panic_cycle = args
        .iter()
        .position(|arg| arg == "--panic-cycle")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u32>().ok());

    for cycle in 0..cycles {
        let backend = match FullscreenBackend::start() {
            Ok(backend) => backend,
            Err(error) => {
                eprintln!("start failed at cycle {cycle}: {error}");
                return ExitCode::from(1);
            }
        };
        if panic_cycle == Some(cycle) {
            panic!("synthetic panic at cycle {cycle}");
        }
        if let Err(error) = backend.shutdown() {
            eprintln!("shutdown failed at cycle {cycle}: {error}");
            return ExitCode::from(1);
        }
    }
    println!("cycles={cycles}");
    ExitCode::SUCCESS
}

fn composer_probe() -> ExitCode {
    let backend = match FullscreenBackend::start() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("composer probe start failed: {error}");
            return ExitCode::from(1);
        }
    };
    let mut composer = Composer::default();
    let result = loop {
        match poll(Duration::from_millis(250)) {
            Ok(true) => match read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char(character) => composer.insert_text(character.to_string()),
                    KeyCode::Enter => composer.insert_text("\n"),
                    KeyCode::Esc => break Ok((composer.payload(), composer.display())),
                    KeyCode::Backspace => {}
                    _ => {}
                },
                Ok(Event::Paste(payload)) => {
                    composer.paste(payload);
                }
                Ok(_) => {}
                Err(error) => break Err(error.to_string()),
            },
            Ok(false) => {}
            Err(error) => break Err(error.to_string()),
        }
    };
    let shutdown_result = backend.shutdown().map_err(|error| error.to_string());
    match (result, shutdown_result) {
        (Ok((payload, display)), Ok(())) => {
            println!("probe_payload={payload}");
            println!("probe_display={display}");
            ExitCode::SUCCESS
        }
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("composer probe failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn long_session_probe() -> ExitCode {
    let mut backend = match FullscreenBackend::start() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("long-session probe start failed: {error}");
            return ExitCode::from(1);
        }
    };
    let mut state = AppState::new();
    for index in 0..3_200 {
        state.blocks.push(Block::new(
            format!("probe-{index}"),
            BlockKind::Assistant("x".repeat(1_600)),
            BlockLifecycle::Complete,
        ));
    }
    let mut samples = Vec::with_capacity(20);
    for _ in 0..20 {
        let start = std::time::Instant::now();
        let draw_result = backend.terminal().draw(|frame| {
            let text = ViewModel::derive(&state).lines.join("\n");
            frame.render_widget(ratatui::widgets::Paragraph::new(text), frame.area());
        });
        if let Err(error) = draw_result {
            let _ = backend.shutdown();
            eprintln!("long-session draw failed: {error}");
            return ExitCode::from(1);
        }
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 19) / 20];
    if let Err(error) = backend.shutdown() {
        eprintln!("long-session shutdown failed: {error}");
        return ExitCode::from(1);
    }
    println!("terminal_long_session_rows=3200 p50_ms={p50} p95_ms={p95}");
    ExitCode::SUCCESS
}

fn event_debug() -> ExitCode {
    let backend = match FullscreenBackend::start() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("event debug start failed: {error}");
            return ExitCode::from(1);
        }
    };
    let mut events = Vec::new();
    loop {
        match read() {
            Ok(event @ Event::Key(key)) => {
                events.push(format!("{event:?}"));
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc {
                    break;
                }
            }
            Ok(event) => events.push(format!("{event:?}")),
            Err(error) => {
                eprintln!("event debug read failed: {error}");
                break;
            }
        }
    }
    if let Err(error) = backend.shutdown() {
        eprintln!("event debug shutdown failed: {error}");
        return ExitCode::from(1);
    }
    for event in events {
        println!("event={event}");
    }
    ExitCode::SUCCESS
}
