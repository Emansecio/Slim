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
                let column = unicode_width::UnicodeWidthStr::width(&row[..x]);
                return (self.foreground[y][column], self.modifiers[y][column]);
            }
        }
        panic!("word not rendered: {word}");
    }

    fn word_bg(&self, word: &str) -> Color {
        for (y, row) in self.rows.iter().enumerate() {
            if let Some(x) = row.find(word) {
                let column = unicode_width::UnicodeWidthStr::width(&row[..x]);
                return self.background[y][column];
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
    render_state_with_caps(state, width, caps())
}

fn render_state_with_caps(state: &AppState, width: u16, capabilities: Capabilities) -> Rendered {
    let backend = TestBackend::new(width, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, state, capabilities, &mut WrapCache::default()))
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
        let mut text_x = 0;
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if x >= text_x {
                row.push_str(cell.symbol());
                text_x = x + (unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1) as u16);
            }
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
        Color::Rgb(0x72, 0xCC, 0x91)
    );
    assert_eq!(
        rendered.word_style("Structure").0,
        Color::Rgb(0x91, 0xC2, 0x8F)
    );
    assert_eq!(
        rendered.word_style("Detail").0,
        Color::Rgb(0xAF, 0xA9, 0x9D)
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
        Color::Rgb(0x72, 0xCC, 0x91),
        "inline paths must stay readable on near-black"
    );
    assert_ne!(
        rendered.word_style("target").0,
        Color::Rgb(0x68, 0x66, 0x5A)
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
        Color::Rgb(0x0A, 0x0A, 0x0A),
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
    assert_eq!(rendered.word_bg("old"), Color::Rgb(0x2A, 0x18, 0x16));
    assert_eq!(rendered.word_bg("new"), Color::Rgb(0x17, 0x2A, 0x1E));
    assert_eq!(rendered.word_bg("context"), Color::Rgb(0x0A, 0x0A, 0x0A));
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
fn narrow_tables_stack_fields_without_dropping_cell_tails() {
    let markdown = format!(
        "| Field | Value |\n| --- | --- |\n| alpha | {} tail-alpha |\n| beta | {} tail-beta |",
        "first-value ".repeat(12),
        "second-value ".repeat(12),
    );
    let rendered = render(&markdown, 24);
    let text = rendered.text();
    assert!(text.contains("Field: alpha"), "{text}");
    assert!(text.contains("Field: beta"), "{text}");
    assert!(
        text.contains("tail-alpha"),
        "first cell tail was lost:\n{text}"
    );
    assert!(
        text.contains("tail-beta"),
        "second cell tail was lost:\n{text}"
    );
}

#[test]
fn narrow_unicode_tables_keep_graphemes_and_sanitized_text() {
    let markdown = "| Campo | Conteúdo |\n| --- | --- |\n| chave | 東京 👨‍👩‍👧‍👦 fim-unicode\u{1b}]52;c2VjcmV0\u{7} |";
    let rendered = render(markdown, 16);
    let text = rendered.text();
    assert!(text.contains("Campo: chave"), "{text}");
    assert!(text.contains("東京"), "wide Unicode cell was lost:\n{text}");
    assert!(text.contains("👨‍👩‍👧‍👦"), "grapheme cell was lost:\n{text}");
    assert!(
        text.contains("fim-unicode"),
        "Unicode tail was lost:\n{text}"
    );
    assert!(!text.contains("52;c2VjcmV0"), "{text}");
}

#[test]
fn wide_tables_keep_column_alignment() {
    let rendered = render(
        "| Pasta | Tamanho |\n| --- | --- |\n| target | 55 GB |\n| .git | 80 MB |",
        80,
    );
    let header = rendered
        .rows
        .iter()
        .find(|row| row.contains("Pasta"))
        .expect("table header");
    let row = rendered
        .rows
        .iter()
        .find(|row| row.contains("target"))
        .expect("table row");
    assert_eq!(header.find("Pasta"), row.find("target"));
    assert_eq!(header.find("Tamanho"), row.find("55 GB"));
    assert!(!rendered.text().contains("Pasta:"), "{header}");
}

#[test]
fn assistant_body_uses_role_label_and_foreground() {
    for lifecycle in [
        BlockLifecycle::Streaming,
        BlockLifecycle::Complete,
        BlockLifecycle::Cancelled,
        BlockLifecycle::Failed,
    ] {
        let mut state = AppState::new();
        assert!(state.append_block(Block::new(
            "answer",
            BlockKind::Assistant("plain answer".into()),
            lifecycle
        )));
        for color_depth in [ColorDepth::TrueColor, ColorDepth::None] {
            let rendered = render_state_with_caps(
                &state,
                40,
                Capabilities {
                    color_depth,
                    ..caps()
                },
            );
            let label = rendered
                .rows
                .iter()
                .position(|row| row.contains("Slim"))
                .expect("agent role");
            assert!(rendered.rows[label - 1].trim().is_empty());
            assert!(rendered.rows[label + 1].contains("plain answer"));
            assert_eq!(
                HeightIndex::build(state.blocks(), 40, &mut WrapCache::default()).total_rows,
                3
            );
            if color_depth == ColorDepth::TrueColor {
                assert_eq!(rendered.word_style("plain").0, Color::Rgb(0xE8, 0xE5, 0xDB));
                if matches!(
                    lifecycle,
                    BlockLifecycle::Streaming | BlockLifecycle::Complete
                ) {
                    assert_eq!(rendered.word_style("Slim").0, Color::Rgb(0x72, 0xCC, 0x91));
                    assert!(rendered.word_style("Slim").1.contains(Modifier::BOLD));
                }
            }
            assert_eq!(
                rendered.text().contains("interrupted · partial"),
                lifecycle == BlockLifecycle::Cancelled
            );
        }
    }
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
    let answer = rendered
        .rows
        .iter()
        .position(|row| row.contains("answer"))
        .expect("answer");
    assert_eq!(
        question, you,
        "You prefixes the prompt on the same band row"
    );
    assert!(rendered.rows[you + 1].trim().is_empty());
    assert!(
        rendered.rows[you + 2].trim().is_empty(),
        "agent breathing row"
    );
    assert_eq!(rendered.rows[you + 3].trim(), "Slim");
    assert_eq!(answer, you + 4);
}

#[test]
fn agent_response_is_separated_from_thinking_and_explicitly_attributed() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded { text: "oi".into() });
    state.apply_event(UiEvent::ThinkingStarted);
    state.apply_event(UiEvent::ThinkingDelta {
        text: "plan".into(),
    });
    state.apply_event(UiEvent::ThinkingEnded);
    state.apply_event(UiEvent::AssistantDelta {
        text: "Oi! Tudo certo?".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    let rendered = render_state(&state, 80);
    let thought = rendered
        .rows
        .iter()
        .position(|row| row.contains("Thought"))
        .expect("thought");
    assert!(rendered.rows[thought + 1].trim().is_empty());
    assert_eq!(rendered.rows[thought + 2].trim(), "Slim");
    assert_eq!(rendered.rows[thought + 3].trim(), "Oi! Tudo certo?");
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
        Color::Rgb(0x99, 0x97, 0x8E)
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
fn user_prompt_label_stays_distinct_from_the_transcript_surface() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UserMessageAdded {
        text: "question".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "answer".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    let rendered = render_state(&state, 80);
    assert_eq!(rendered.word_bg("You"), Color::Rgb(0x10, 0x10, 0x10));
    assert_eq!(rendered.word_bg("question"), Color::Rgb(0x10, 0x10, 0x10));
    assert_ne!(rendered.word_bg("answer"), Color::Rgb(0x10, 0x10, 0x10));
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
        Color::Rgb(0x99, 0x97, 0x8E)
    );
    assert_ne!(rendered.word_bg("queued[0]"), Color::Rgb(0x10, 0x10, 0x10));
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

#[test]
fn restored_tool_is_neutral_collapsed_and_inspectable_in_fullscreen() {
    use slim_tui::api::{SessionId, ToolBatchId, ToolCallId, TranscriptMessage, TranscriptRole};
    for width in [44, 100] {
        let mut state = AppState::new();
        let effects = reduce(
            &mut state,
            Action::UiEventReceived(UiEvent::SessionRestored {
                session_id: SessionId("history".into()),
                cwd: "workspace".into(),
                messages: vec![TranscriptMessage {
                    role: TranscriptRole::Tool {
                        batch_id: ToolBatchId("saved-batch".into()),
                        call_id: ToolCallId("saved-call".into()),
                        name: "shell".into(),
                        arguments: "{\"command\":\"cargo test\"}".into(),
                    },
                    text: "error: test failed\nsecond line".into(),
                }],
                skill_names: Vec::new(),
            }),
        );
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, slim_tui::reducer::Effect::Send(_))));
        let block = &state.blocks()[0];
        assert_eq!(block.fold, FoldState::Collapsed);
        let id = block.id.clone();
        let frame = render_state(&state, width).text();
        assert!(frame.contains("shell · history"), "{frame}");
        assert!(
            !frame.contains("✓ shell") && !frame.contains("cargo test"),
            "{frame}"
        );
        let effects = reduce(&mut state, Action::ToggleBlock(id));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, slim_tui::reducer::Effect::Send(_))));
        let frame = render_state(&state, width).text();
        for text in [
            "Arguments:",
            "cargo test",
            "Result (saved):",
            "error: test failed",
            "second line",
        ] {
            assert!(frame.contains(text), "missing {text}: {frame}");
        }
        assert!(!state.working);
        assert_eq!(state.tools_used_read, 0);
        assert_eq!(state.tools_used_mutating, 0);
    }
}
