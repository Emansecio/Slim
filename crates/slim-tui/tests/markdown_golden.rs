use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use slim_tui::api::{TodoItemStatus, TodoItemView, UiEvent};
use slim_tui::app::{AppState, LoginOverlay};
use slim_tui::block::{Block, BlockKind, BlockLifecycle, FoldState, ToolState};
use slim_tui::reducer::{reduce, Action};
use slim_tui::render::{render as render_plain_frame, HeightIndex, WrapCache};
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

#[derive(Debug)]
struct Rendered {
    rows: Vec<String>,
    foreground: Vec<Vec<Color>>,
    background: Vec<Vec<Color>>,
    modifiers: Vec<Vec<Modifier>>,
}

impl Rendered {
    fn text(&self) -> String {
        self.rows.join("\n")
    }

    fn word_style(&self, word: &str) -> (Color, Modifier) {
        for (y, row) in self.rows.iter().enumerate() {
            if let Some(x) = row.find(word) {
                return (self.foreground[y][x], self.modifiers[y][x]);
            }
        }
        panic!("word not rendered: {word}");
    }

    fn word_bg(&self, word: &str) -> Color {
        for (y, row) in self.rows.iter().enumerate() {
            if let Some(x) = row.find(word) {
                return self.background[y][x];
            }
        }
        panic!("word not rendered: {word}");
    }
}

fn caps() -> Capabilities {
    Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    }
}

fn render(markdown: &str, width: u16) -> Rendered {
    let mut state = AppState::new();
    reduce(
        &mut state,
        Action::UiEventReceived(UiEvent::AssistantDelta {
            text: markdown.into(),
        }),
    );
    render_state(&state, width)
}

fn render_state(state: &AppState, width: u16) -> Rendered {
    let backend = TestBackend::new(width, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, state, caps(), &mut WrapCache::default()))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut rows = Vec::new();
    let mut foreground = Vec::new();
    let mut background = Vec::new();
    let mut modifiers = Vec::new();
    for y in 0..buffer.area.height {
        let mut row = String::new();
        let mut row_foreground = Vec::new();
        let mut row_background = Vec::new();
        let mut row_modifiers = Vec::new();
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            row.push_str(cell.symbol());
            row_foreground.push(cell.fg);
            row_background.push(cell.bg);
            row_modifiers.push(cell.modifier);
        }
        rows.push(row);
        foreground.push(row_foreground);
        background.push(row_background);
        modifiers.push(row_modifiers);
    }
    Rendered {
        rows,
        foreground,
        background,
        modifiers,
    }
}

#[test]
fn headings_hide_markers_and_use_three_semantic_colors() {
    let rendered = render("# Primary\n## Structure\n### Detail", 80);
    let text = rendered.text();
    assert!(!text.contains("# Primary"));
    assert!(!text.contains("## Structure"));
    assert!(!text.contains("### Detail"));
    assert_eq!(
        rendered.word_style("Primary").0,
        Color::Rgb(0x78, 0xD9, 0x9B)
    );
    assert_eq!(
        rendered.word_style("Structure").0,
        Color::Rgb(0x82, 0xAF, 0xFF)
    );
    assert_eq!(
        rendered.word_style("Detail").0,
        Color::Rgb(0x9A, 0xA4, 0xAF)
    );
}

#[test]
fn inline_markdown_renders_without_control_markers() {
    let rendered = render(
        "- one\n- **strong** and *emphasis* with `code` and [link](https://example.com)\n> quote",
        80,
    );
    let text = rendered.text();
    for marker in ["**strong**", "*emphasis*", "`code`", "]("] {
        assert!(!text.contains(marker), "raw marker {marker}");
    }
    for content in ["one", "strong", "emphasis", "code", "link", "quote"] {
        assert!(text.contains(content), "missing {content}");
    }
    assert!(!text.contains("https://example.com"));
    assert!(
        rendered.word_style("strong").1.contains(Modifier::BOLD),
        "strong text must be bold"
    );
}

#[test]
fn inline_code_uses_tool_accent_not_code_rail() {
    let rendered = render("see `target\\debug` and more", 80);
    assert_eq!(
        rendered.word_style("target").0,
        Color::Rgb(0x7D, 0xCF, 0xFF),
        "inline paths must stay readable on near-black"
    );
    assert_ne!(
        rendered.word_style("target").0,
        Color::Rgb(0x46, 0x50, 0x5C)
    );
}

#[test]
fn fenced_code_uses_rail_and_code_background() {
    let rendered = render("```rust\nlet value = 1;\n```", 80);
    assert!(
        rendered.text().contains("│ let value = 1;"),
        "fenced code must use a functional rail:\n{}",
        rendered.text()
    );
    assert_eq!(
        rendered.word_bg("value"),
        Color::Rgb(0x0B, 0x0E, 0x12),
        "fenced code must use code_bg"
    );
}

#[test]
fn diff_rows_keep_prefixes_and_semantic_backgrounds() {
    let rendered = render("```diff\n-old value\n+new value\n context\n```", 80);
    let text = rendered.text();
    assert!(
        text.contains("│ -old value"),
        "remove prefix/rail missing:\n{text}"
    );
    assert!(
        text.contains("│ +new value"),
        "add prefix/rail missing:\n{text}"
    );
    assert_eq!(rendered.word_bg("old"), Color::Rgb(0x1C, 0x0D, 0x11));
    assert_eq!(rendered.word_bg("new"), Color::Rgb(0x0B, 0x1A, 0x10));
    assert_eq!(rendered.word_bg("context"), Color::Rgb(0x0B, 0x0E, 0x12));
}

#[test]
fn nested_lists_keep_child_on_own_row() {
    let rendered = render("- parent\n  - child", 40);
    let parent_row = rendered
        .rows
        .iter()
        .position(|row| row.contains("parent"))
        .expect("parent row");
    let child_row = rendered
        .rows
        .iter()
        .position(|row| row.contains("child"))
        .expect("child row");
    assert_ne!(parent_row, child_row);
}

#[test]
fn incomplete_streaming_markdown_stays_visible() {
    let text = render("## Stable\n**unfinished", 40).text();
    assert!(text.contains("Stable"));
    assert!(text.contains("unfinished"));
}

#[test]
fn assistant_body_uses_role_label_and_foreground() {
    let rendered = render("plain answer", 40);
    let text = rendered.text();
    assert!(text.contains("Slim"), "{text}");
    assert_eq!(rendered.word_style("plain").0, Color::Rgb(0xC6, 0xCD, 0xD5));
}

#[test]
fn user_band_is_followed_by_one_blank_row() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "question".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "answer".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    let rendered = render_state(&state, 80);
    let you = rendered
        .rows
        .iter()
        .position(|row| row.contains("You"))
        .expect("You");
    let question = rendered
        .rows
        .iter()
        .position(|row| row.contains("question"))
        .expect("question");
    let slim = rendered
        .rows
        .iter()
        .position(|row| row.contains("Slim"))
        .expect("Slim");
    let answer = rendered
        .rows
        .iter()
        .position(|row| row.contains("answer"))
        .expect("answer");
    assert_eq!(question, you + 1);
    assert!(rendered.rows[question + 1].trim().is_empty());
    assert_eq!(slim, question + 2);
    assert_eq!(answer, slim + 1);
}

#[test]
fn collapsed_thinking_is_single_muted_metadata_row() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "secret plan\nmore".into(),
    });
    state.apply_event(UiEvent::ThinkingEnded);
    let rendered = render_state(&state, 80);
    let text = rendered.text();
    assert!(text.contains("Thought"), "{text}");
    assert!(!text.contains("secret plan"), "{text}");
    assert!(!text.contains("more"), "{text}");
    assert_eq!(
        rendered.word_style("Thought").0,
        Color::Rgb(0x74, 0x7B, 0x84)
    );
}

#[test]
fn streaming_thinking_shows_only_the_latest_two_physical_rows() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "first hidden line\nsecond live line\nfinal live line".into(),
    });

    let streaming = render_state(&state, 48).text();
    assert!(streaming.contains("Thinking"), "{streaming}");
    assert!(!streaming.contains("first hidden line"), "{streaming}");
    assert!(streaming.contains("… second live line"), "{streaming}");
    assert!(streaming.contains("final live line"), "{streaming}");

    state.apply_event(UiEvent::ThinkingEnded);
    let complete = render_state(&state, 48).text();
    assert!(complete.contains("Thought"), "{complete}");
    assert!(!complete.contains("second live line"), "{complete}");
    assert!(!complete.contains("final live line"), "{complete}");
}

#[test]
fn user_prompt_band_is_elevated_against_assistant_surface() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "question".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "answer".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    let rendered = render_state(&state, 80);
    assert_eq!(rendered.word_bg("You"), Color::Rgb(0x1B, 0x20, 0x26));
    assert_eq!(rendered.word_bg("question"), Color::Rgb(0x1B, 0x20, 0x26));
    assert_ne!(rendered.word_bg("answer"), Color::Rgb(0x1B, 0x20, 0x26));
}

#[test]
fn queued_user_stays_muted_without_prompt_surface() {
    let mut state = AppState::new();
    assert!(state.append_block(Block::new(
        "queued-1",
        BlockKind::QueuedUser("queued[0] later".into()),
        BlockLifecycle::Complete,
    )));
    let rendered = render_state(&state, 80);
    assert_eq!(
        rendered.word_style("queued[0]").0,
        Color::Rgb(0x74, 0x7B, 0x84)
    );
    assert_ne!(rendered.word_bg("queued[0]"), Color::Rgb(0x1B, 0x20, 0x26));
}

#[test]
fn terminal_control_payload_is_not_materialized() {
    let text = render("safe\u{1b}]52;c2VjcmV0\u{7}tail", 80).text();
    assert!(text.contains("safetail"));
    assert!(!text.contains("52;c2VjcmV0"));
}

#[test]
fn every_untrusted_block_kind_strips_terminal_controls() {
    let payload = "safe\u{1b}]52;c2VjcmV0\u{7}tail";
    let kinds = vec![
        BlockKind::User(payload.into()),
        BlockKind::Thinking(payload.into()),
        BlockKind::Tool(ToolState {
            name: payload.into(),
            preview: payload.into(),
            ..ToolState::default()
        }),
        BlockKind::System(payload.into()),
        BlockKind::Error(payload.into()),
        BlockKind::Activity(payload.into()),
        BlockKind::QueuedUser(payload.into()),
    ];
    for (index, kind) in kinds.into_iter().enumerate() {
        let mut state = AppState::new();
        let mut block = Block::new(format!("unsafe-{index}"), kind, BlockLifecycle::Streaming);
        if matches!(block.kind(), BlockKind::Thinking(_)) {
            block.fold = FoldState::Expanded;
        }
        assert!(state.append_block(block));
        let text = render_state(&state, 80).text();
        assert!(text.contains("safetail"), "kind {index} lost safe text");
        assert!(
            !text.contains("52;c2VjcmV0"),
            "kind {index} exposed OSC payload"
        );
    }
}

#[test]
fn composer_and_model_label_strip_terminal_controls() {
    let payload = "safe\u{1b}]52;c2VjcmV0\u{7}tail";
    let mut state = AppState::new();
    state.composer.insert_text(payload);
    state.model = payload.into();
    let text = render_state(&state, 80).text();
    assert!(text.contains("safetail"));
    assert!(!text.contains("52;c2VjcmV0"));
}

#[test]
fn login_overlay_strips_terminal_controls() {
    let payload = "safe\u{1b}]52;c2VjcmV0\u{7}tail";
    let mut state = AppState::new();
    state.login_overlay = Some(LoginOverlay {
        progress: Some(payload.into()),
        ..LoginOverlay::default()
    });
    let text = render_state(&state, 80).text();
    assert!(text.contains("safetail"));
    assert!(!text.contains("52;c2VjcmV0"));
}

#[test]
fn public_plain_frame_strips_terminal_controls() {
    let payload = "safe\u{1b}]52;c2VjcmV0\u{7}tail";
    let mut state = AppState::new();
    let kinds = [
        BlockKind::User(payload.into()),
        BlockKind::Assistant(payload.into()),
        BlockKind::Thinking(payload.into()),
        BlockKind::Tool(ToolState {
            name: payload.into(),
            preview: payload.into(),
            ..ToolState::default()
        }),
        BlockKind::System(payload.into()),
        BlockKind::Error(payload.into()),
        BlockKind::Activity(payload.into()),
        BlockKind::QueuedUser(payload.into()),
    ];
    for (index, kind) in kinds.into_iter().enumerate() {
        assert!(state.append_block(Block::new(
            format!("plain-safe-{index}"),
            kind,
            BlockLifecycle::Streaming,
        )));
    }
    state.notifications.push(payload.into());
    state.todo_dock_open = true;
    state.todo_items.push(TodoItemView {
        title: payload.into(),
        status: TodoItemStatus::InProgress,
    });
    state.composer.insert_text(payload);

    let text = render_plain_frame(&state, 80, 24).lines.join("\n");
    assert!(text.contains("safetail"));
    assert!(!text.contains("52;c2VjcmV0"));
}

#[test]
fn duplicate_block_id_cannot_replace_cached_content() {
    let mut state = AppState::new();
    assert!(state.append_block(Block::new(
        "stable-id",
        BlockKind::User("first".into()),
        BlockLifecycle::Complete,
    )));
    assert!(!state.append_block(Block::new(
        "stable-id",
        BlockKind::User("replacement".into()),
        BlockLifecycle::Complete,
    )));
    assert!(matches!(
        state.blocks()[0].kind(),
        BlockKind::User(text) if text == "first"
    ));

    state.apply_event(UiEvent::UserMessageAdded {
        text: "generated".into(),
    });
    assert_eq!(state.blocks().len(), 2);
    assert_ne!(state.blocks()[0].id, state.blocks()[1].id);
}

#[test]
fn cross_state_same_id_never_reuses_stale_height() {
    let mut first = AppState::new();
    assert!(first.append_block(Block::new(
        "shared-id",
        BlockKind::Assistant("short".into()),
        BlockLifecycle::Complete,
    )));
    let mut second = AppState::new();
    assert!(second.append_block(Block::new(
        "shared-id",
        BlockKind::Assistant("long line ".repeat(80)),
        BlockLifecycle::Complete,
    )));

    let mut cache = WrapCache::default();
    let short = HeightIndex::build(first.blocks(), 20, &mut cache).total_rows;
    let long = HeightIndex::build(second.blocks(), 20, &mut cache).total_rows;
    assert!(
        long > short,
        "distinct block instances need distinct cache keys"
    );
}

#[test]
fn diverged_clones_never_share_height_identity() {
    let mut first = AppState::new();
    assert!(first.append_block(Block::new(
        "clone-id",
        BlockKind::Assistant("base".into()),
        BlockLifecycle::Streaming,
    )));
    let mut second = first.clone();
    first.apply_event(UiEvent::AssistantDelta { text: " x".into() });
    second.apply_event(UiEvent::AssistantDelta {
        text: " very long".repeat(80),
    });

    let mut cache = WrapCache::default();
    let short = HeightIndex::build(first.blocks(), 20, &mut cache).total_rows;
    let long = HeightIndex::build(second.blocks(), 20, &mut cache).total_rows;
    assert!(
        long > short,
        "diverged clones need independent cache identities"
    );
}

#[test]
fn cancellation_and_deferred_end_target_active_lifecycles() {
    let mut cancelled = AppState::new();
    cancelled.apply_event(UiEvent::ToolStarted {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("read".into()),
        name: "read".into(),
        arguments_summary: String::new(),
    });
    cancelled.apply_event(UiEvent::RunCancelled { run_id: 1 });
    assert_eq!(cancelled.blocks()[0].lifecycle, BlockLifecycle::Cancelled);

    let mut late_start = AppState::new();
    late_start.apply_event(UiEvent::RunCancelled { run_id: 1 });
    late_start.apply_event(UiEvent::ToolStarted {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("write".into()),
        name: "write".into(),
        arguments_summary: String::new(),
    });
    late_start.apply_event(UiEvent::ToolEnded {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("write".into()),
        name: "write".into(),
        success: false,
        duration_ms: 1,
    });
    assert_eq!(late_start.blocks()[0].lifecycle, BlockLifecycle::Cancelled);

    let mut ordered = AppState::new();
    ordered.apply_event(UiEvent::AssistantDelta {
        text: "answer".into(),
    });
    ordered.apply_event(UiEvent::ToolStarted {
        batch_id: slim_tui::api::ToolBatchId("batch".into()),
        call_id: slim_tui::api::ToolCallId("read".into()),
        name: "read".into(),
        arguments_summary: String::new(),
    });
    ordered.apply_event(UiEvent::AssistantEnded);
    assert_eq!(ordered.blocks()[0].lifecycle, BlockLifecycle::Complete);
    assert_eq!(ordered.blocks()[1].lifecycle, BlockLifecycle::Streaming);
}

#[test]
fn command_palette_query_strips_terminal_controls() {
    let mut state = AppState::new();
    state.palette_query = Some("safe\u{1b}]52;c2VjcmV0\u{7}tail".into());
    let text = render_state(&state, 80).text();
    assert!(text.contains("safetail"));
    assert!(!text.contains("52;c2VjcmV0"));
}

#[test]
fn command_palette_groups_session_and_runtime() {
    let mut state = AppState::new();
    state.palette_query = Some(String::new());
    let text = render_state(&state, 80).text();
    assert!(text.contains("session"), "{text}");
    assert!(text.contains("runtime"), "{text}");
    assert!(text.contains("/login"), "{text}");
    assert!(text.contains("/mode"), "{text}");
}

#[test]
fn bounded_cache_does_not_cascade_miss_above_capacity() {
    let mut state = AppState::new();
    for index in 0..16_385 {
        assert!(state.append_block(Block::new(
            format!("cache-{index}"),
            BlockKind::User("row".into()),
            BlockLifecycle::Complete,
        )));
    }
    let mut cache = WrapCache::default();
    HeightIndex::build(state.blocks(), 80, &mut cache);
    let before = cache.height_misses();
    HeightIndex::build(state.blocks(), 80, &mut cache);
    assert_eq!(cache.height_misses() - before, 1);
}

#[test]
fn completed_singleton_tool_is_success_on_both_surfaces() {
    let mut state = AppState::new();
    assert!(state.append_block(Block::new(
        "tool-success",
        BlockKind::Tool(ToolState {
            name: "read".into(),
            preview: "done".into(),
            ..ToolState::default()
        }),
        BlockLifecycle::Complete,
    )));

    let fullscreen = render_state(&state, 80).text();
    let plain = render_plain_frame(&state, 80, 24).lines.join("\n");
    assert!(fullscreen.contains("✓ read"), "{fullscreen}");
    assert!(plain.contains("✓ read"), "{plain}");
    assert!(!plain.contains("failed"), "{plain}");
}

#[test]
fn streaming_delta_invalidates_cached_height() {
    let mut cache = WrapCache::default();
    let mut block = Block::new(
        "assistant-1",
        BlockKind::Assistant("short".into()),
        BlockLifecycle::Streaming,
    );
    let first = HeightIndex::build(std::slice::from_ref(&block), 20, &mut cache).total_rows;
    block.append_text("\nsecond row\nthird row");
    let second = HeightIndex::build(std::slice::from_ref(&block), 20, &mut cache).total_rows;
    assert!(
        second > first,
        "streaming height stayed stale: {first} → {second}"
    );
}
