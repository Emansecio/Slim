//! Layout goldens for the boxed composer and typed Todo dock (gates B1s,
//! B4s, B5s, W8): one aligned three-row composer box at every supported size,
//! responsive footer content, dock progress and active item.

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use slim_tui::api::{SensitiveText, TodoItemStatus, TodoItemView, UiEvent};
use slim_tui::app::{AppState, FollowMode, LoginOverlay, LoginStage};
use slim_tui::layout::{plan, plan_with_session_rail};
use slim_tui::reducer::reduce;
use slim_tui::render::WrapCache;
use slim_tui::runtime::render_frame;
use slim_tui::theme::{Capabilities, ColorDepth};

fn caps() -> Capabilities {
    Capabilities {
        color_depth: ColorDepth::TrueColor,
        mouse: false,
        clipboard: false,
        images: false,
        reduced_motion: false,
    }
}

fn render_to_string(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, state, caps(), &mut WrapCache::default()))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        out.push('\n');
    }
    out
}

fn corner_columns(line: &str, left: char, right: char) -> (usize, usize) {
    let left = line
        .chars()
        .position(|glyph| glyph == left)
        .expect("left corner");
    let right = line
        .chars()
        .position(|glyph| glyph == right)
        .expect("right corner");
    (left, right)
}

fn conversation_with_context() -> AppState {
    let mut state = AppState::new();
    state.cwd = r"D:\Slim".into();
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 9_500,
        context_window_tokens: 128_000,
    });
    state.apply_event(UiEvent::UserMessageAdded {
        text: "question".into(),
    });
    state.apply_event(UiEvent::AssistantDelta {
        text: "hello".into(),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state
}

fn todo_state() -> AppState {
    let mut state = AppState::new();
    reduce(
        &mut state,
        slim_tui::reducer::Action::UiEventReceived(UiEvent::TodoChanged {
            items: vec![
                TodoItemView {
                    title: "mapear fluxo".into(),
                    status: TodoItemStatus::Completed,
                },
                TodoItemView {
                    title: "rodar testes".into(),
                    status: TodoItemStatus::InProgress,
                },
                TodoItemView {
                    title: "revisar diff".into(),
                    status: TodoItemStatus::Pending,
                },
            ],
        }),
    );
    state
}

#[test]
fn verbatim_windows_cwd_is_not_rendered_raw() {
    let mut state = conversation_with_context();
    state.cwd = r"\\?\D:\Slim".into();
    let frame = render_to_string(&state, 100, 24);
    let first = frame.lines().next().expect("session header");
    assert!(
        !first.contains(r"\\?\"),
        "verbatim prefix must not appear\n{first}"
    );
    assert!(first.contains(r"D:\Slim"), "header={first:?}");
}

#[test]
fn session_rail_keeps_provider_phase_out_of_session_header() {
    let mut state = conversation_with_context();
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::ProviderPhaseChanged {
        phase: slim_core::ProviderPhase::Connecting,
        label: "Connecting to provider".into(),
        elapsed_ms: 0,
    });
    let frame = render_to_string(&state, 100, 24);
    let header = frame.lines().next().expect("session header");
    assert!(!header.contains("RUNNING"), "header={header:?}");
    assert!(
        !header.contains("Connecting to provider"),
        "phase belongs on the activity rail\n{header}"
    );
    assert!(frame.contains("Connecting to provider"), "{frame}");
    assert_eq!(
        frame.matches("Connecting to provider").count(),
        1,
        "phase must appear once on the activity rail\n{frame}"
    );
    assert!(
        !frame.contains("Thinking"),
        "provider plumbing is not reasoning\n{frame}"
    );
}

#[test]
fn session_rail_is_conversation_only() {
    let mut welcome = AppState::new();
    welcome.cwd = r"D:\Slim".into();
    let welcome_frame = render_to_string(&welcome, 100, 24);
    assert!(
        !welcome_frame
            .lines()
            .next()
            .unwrap_or_default()
            .contains("SLIM"),
        "no session rail on the welcome screen\n{welcome_frame}"
    );

    let conversation = conversation_with_context();
    let frame = render_to_string(&conversation, 100, 24);
    assert!(frame.contains(r"D:\Slim"));
    assert!(frame.lines().next().unwrap_or_default().contains("SLIM"));
}

#[test]
fn hidden_session_rail_moves_context_to_footer() {
    let state = conversation_with_context();
    let narrow = render_to_string(&state, 79, 12);
    let footer = narrow.lines().rev().take(2).collect::<Vec<_>>();
    assert!(
        footer.iter().any(|line| line.contains("ctx ~7%")),
        "context belongs to the two-row footer: {footer:?}"
    );

    let too_short = render_to_string(&state, 80, 11);
    assert_eq!(too_short.matches("ctx ~7%").count(), 1);
    assert!(!too_short
        .lines()
        .next()
        .unwrap_or_default()
        .contains(r"D:\Slim"));
    assert!(too_short
        .lines()
        .rev()
        .take(1)
        .any(|line| line.contains("ctx ~7%")));
}

#[test]
fn trivial_cwd_keeps_status_header_without_rendering_the_path() {
    for cwd in [
        "~".to_owned(),
        std::env::var("USERPROFILE").unwrap_or_else(|_| "~".to_owned()),
    ] {
        let mut state = conversation_with_context();
        state.cwd = cwd;
        let frame = render_to_string(&state, 100, 24);
        let first = frame.lines().next().expect("first row");

        assert!(
            !first.contains("SLIM"),
            "trivial cwd has no session rail\n{frame}"
        );
        assert_eq!(
            frame
                .lines()
                .filter(|line| line.contains("ctx ~7%"))
                .count(),
            1,
            "context remains in footer\n{frame}"
        );
        assert_eq!(frame.matches("ctx ~7%").count(), 1, "{frame}");
        assert!(frame
            .lines()
            .rev()
            .take(2)
            .any(|line| line.contains("ctx ~7%")));
    }
}

#[test]
fn wide_session_rail_keeps_context_in_the_footer_only() {
    let state = conversation_with_context();
    let wide = render_to_string(&state, 100, 24);
    assert_eq!(wide.matches("ctx ~7%").count(), 1);
    assert!(
        !wide
            .lines()
            .next()
            .expect("session rail")
            .contains("ctx ~7%"),
        "header must not keep the context counter"
    );
    let footer = wide.lines().rev().take(2).collect::<Vec<_>>();
    assert!(footer.iter().any(|line| line.contains("ctx ~7%")));
    assert!(footer
        .iter()
        .all(|line| !line.contains('↑') && !line.contains('↓')));
}

#[test]
fn long_wide_cwd_is_cell_truncated_without_touching_context() {
    let mut state = conversation_with_context();
    state.cwd = "D:\\宇宙\\uma-pasta-com-nome-muito-longo\n\\Slim\u{1b}]52;c2VjcmV0\u{7}".into();
    let logical = slim_tui::render::render(&state, 80, 12);
    let logical_rail = logical.lines.first().expect("logical session rail");
    assert!(!logical_rail.contains('\n'));
    assert!(unicode_width::UnicodeWidthStr::width(logical_rail.as_str()) <= 80);
    let wide = render_to_string(&state, 80, 12);
    let rail = wide.lines().next().expect("session rail");
    assert!(!rail.contains("ctx ~7%"), "rail={rail:?}");
    assert!(
        wide.lines()
            .rev()
            .take(2)
            .any(|line| line.contains("ctx ~7%")),
        "{wide}"
    );
    assert!(!rail.contains("c2VjcmV0"), "OSC payload leaked: {rail:?}");
    assert_eq!(
        rail.chars().count(),
        80,
        "TestBackend row stays within the 80-cell rail"
    );
}

#[test]
fn planner_degrades_session_rail_before_activity() {
    let spacious = plan_with_session_rail(100, 24, 0, true, true);
    assert_eq!(spacious.session_rail.height, 1);
    assert_eq!(spacious.activity_rail.height, 1);

    let constrained = plan_with_session_rail(80, 12, 5, true, true);
    assert_eq!(constrained.session_rail.height, 0);
    assert_eq!(
        constrained.activity_rail.height, 1,
        "SessionRail degrades before ActivityRail"
    );
}

#[test]
fn composer_long_line_wraps_without_clipping_payload_or_cursor() {
    let mut state = AppState::new();
    state.authenticated = true;
    let payload = format!("{}{}", "A".repeat(90), "cursor-end");
    state.composer.insert_text(payload.clone());

    let frame = render_to_string(&state, 80, 24);
    let snapshot = state.composer.display_snapshot(74);
    assert_eq!(
        snapshot.total_lines, 2,
        "long draft uses visual soft-wrap rows"
    );
    assert_eq!(
        snapshot.lines.concat(),
        payload,
        "wrapping never mutates text"
    );
    assert_eq!(snapshot.cursor_line, 1, "cursor follows the wrapped tail");
    assert!(
        frame.lines().any(|line| line.contains("cursor-end")),
        "cursor tail remains visible\n{frame}"
    );
    assert!(
        !frame.contains('<'),
        "horizontal overflow hint is unnecessary after wrap"
    );
}

#[test]
fn composer_multiline_expands_and_reports_total_lines_in_the_label() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.composer.insert_text("alpha\nbravo");
    let frame = render_to_string(&state, 80, 24);
    assert!(frame.lines().any(|line| line.contains("alpha")), "{frame}");
    assert!(
        frame.lines().any(|line| line.contains("> bravo")),
        "{frame}"
    );
    assert!(
        frame.contains("2 lines"),
        "line count label missing\n{frame}"
    );
}

#[test]
fn narrow_composer_renderer_keeps_tabs_and_paste_chips_sanitized() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.composer.insert_text("a\tb");
    state.composer.paste("line one\nline two");

    let frame = render_to_string(&state, 40, 12);
    assert!(
        frame.contains("a   b"),
        "tab expands at the renderer boundary\n{frame}"
    );
    assert!(
        frame.contains("Pasted Content"),
        "paste remains an atomic chip\n{frame}"
    );
    assert!(
        !frame.contains('\t'),
        "raw tab must not reach the terminal buffer\n{frame}"
    );
    assert!(
        frame.lines().all(|line| line.chars().count() <= 40),
        "narrow frame stays inside its terminal width\n{frame}"
    );
}

#[test]
fn tokenized_multiline_paste_counts_single_row() {
    // G246: a multipaste rendered as an atomic segment is one logical row, so
    // no line/total indicator appears (raw payload lines are not counted).
    let mut state = AppState::new();
    state.authenticated = true;
    state.composer.paste("line one\nline two\nline three");
    let frame = render_to_string(&state, 80, 24);
    let row = frame
        .lines()
        .find(|line| line.contains("Pasted Content"))
        .unwrap_or_else(|| panic!("paste token row missing\n{frame}"));
    assert!(
        !row.contains('/'),
        "atomic segment must not show raw line counts: {row}"
    );
}

#[test]
fn composer_cursor_sits_on_the_cell_after_the_last_glyph() {
    // G264: the caret must not cover the last typed letter.
    let mut state = AppState::new();
    state.authenticated = true;
    state.composer.insert_text("tela");

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, &state, caps(), &mut WrapCache::default()))
        .expect("draw");
    let cursor = terminal.get_cursor_position().expect("cursor");
    let buffer = terminal.backend().buffer();

    let mut last_a = None;
    for x in 0..buffer.area.width {
        if buffer[(x, cursor.y)].symbol() == "a"
            && x > 0
            && buffer[(x.saturating_sub(1), cursor.y)].symbol() == "l"
        {
            last_a = Some(x);
        }
    }
    let last_a = last_a.expect("last letter of 'tela' on the cursor row");
    assert_eq!(
        cursor.x,
        last_a + 1,
        "cursor must sit after the last glyph (a at {last_a}, cursor at {})\n{}",
        cursor.x,
        {
            let y = cursor.y;
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
        }
    );
}

#[test]
fn multiline_composer_cursor_uses_display_cell_width_for_unicode_tail() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.composer.insert_text("first line\n界z");

    let width = 40;
    let height = 16;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, &state, caps(), &mut WrapCache::default()))
        .expect("draw");

    let snapshot = state.composer.display_snapshot(32);
    assert_eq!(snapshot.cursor_line, 1);
    assert_eq!(snapshot.cursor_cell, 3, "界 occupies two terminal cells");
    let cursor = terminal.get_cursor_position().expect("cursor");
    let buffer = terminal.backend().buffer();
    let cursor_row = (0..buffer.area.width)
        .find(|&x| buffer[(x, cursor.y)].symbol() == "z")
        .expect("tail glyph on cursor row");
    assert_eq!(cursor.x, cursor_row + 1, "cursor follows the final glyph");
    assert!(cursor.x < width, "cursor remains inside the composer width");
}

#[test]
fn boxed_composer_and_todo_dock_at_normal_height() {
    let mut state = todo_state();
    state.authenticated = true;
    state.composer.insert_text("draft text");
    let frame = render_to_string(&state, 80, 24);

    assert!(frame.contains("TODO 1/3"), "dock header with progress");
    assert!(frame.contains("rodar testes"), "active item visible");
    assert!(frame.contains("✓ mapear fluxo"), "completed glyph");
    assert!(frame.contains("○ revisar diff"), "pending glyph");

    // Rounded composer box: model/effort on the footer metadata row; mode and
    // controls occupy the surrounding footer rows.
    assert!(frame.contains("GPT-5.6 Sol (high)"), "composer label");
    let footer = frame.lines().rev().take(2).collect::<Vec<_>>();
    assert!(footer
        .iter()
        .any(|line| line.trim_start().starts_with("Auto")));
    assert!(footer
        .iter()
        .any(|line| line.contains("GPT-5.6 Sol (high)")));
    assert!(footer.iter().any(|line| line.contains("Ctrl+P")));
    let draft_row = frame
        .lines()
        .find(|line| line.contains("> draft text"))
        .expect("draft inside the box");
    assert!(draft_row.contains('─') || frame.lines().any(|l| l.contains('─')));
}

#[test]
fn grok_footer_keeps_aligned_inset_box_and_adjacent_status() {
    for width in [48, 100, 160] {
        let mut state = AppState::new();
        state.authenticated = true;
        state.composer.insert_text("draft");
        let frame = render_to_string(&state, width, 24);
        let lines = frame.lines().collect::<Vec<_>>();
        let top_index = lines
            .iter()
            .position(|line| line.contains('╭'))
            .expect("top border");
        let bottom_index = lines
            .iter()
            .position(|line| line.contains('╰'))
            .expect("bottom border");
        let top = corner_columns(lines[top_index], '╭', '╮');
        let bottom = corner_columns(lines[bottom_index], '╰', '╯');

        assert_eq!(top, bottom, "symmetric corners at width={width}");
        let band_width = width as usize;
        let band_x = 0;
        assert_eq!(
            top,
            (band_x + 1, band_x + band_width - 2),
            "one-cell inset inside the terminal width"
        );
        assert_eq!(
            bottom_index + 1,
            lines.len() - 2,
            "composer is directly above the two-row footer"
        );
        assert!(frame.contains("GPT-5.6 Sol (high)"));
        assert!(frame.contains("Auto"));
        if width >= 80 {
            assert!(frame.contains("Shift+Tab"));
            assert!(frame.contains("Ctrl+P"));
        } else {
            assert!(frame.contains("⇧Tab") || frame.contains("Auto"));
        }
    }
}

#[test]
fn overflowed_billable_totals_stay_out_of_the_main_screen() {
    let mut state = AppState::new();
    state.input_tokens = u64::MAX;
    state.output_tokens = u64::MAX;
    state.input_tokens_overflowed = true;
    state.output_tokens_overflowed = true;
    let frame = render_to_string(&state, 100, 24);
    let footer = frame.lines().last().expect("footer");
    assert!(!footer.contains('↑'));
    assert!(!footer.contains('↓'));
}

#[test]
fn narrow_supported_footer_preserves_context_without_billable_totals() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 90_000,
        context_window_tokens: 100_000,
    });
    state.input_tokens = u64::MAX;
    state.output_tokens = u64::MAX;

    let frame = render_to_string(&state, 40, 8);
    let footer = frame.lines().last().expect("footer");
    assert!(
        footer.contains("ctx ~90%"),
        "compact context remains visible: {footer:?}"
    );
    assert!(!footer.contains('↑'), "input total stays out of footer");
    assert!(!footer.contains('↓'), "output total stays out of footer");
}

#[test]
fn narrow_active_and_pinned_footers_keep_context_and_navigation() {
    let mut active = AppState::new();
    active.working = true;
    active.apply_event(UiEvent::ActivityChanged {
        label: "Working".into(),
    });
    active.apply_event(UiEvent::UsageEstimate {
        request_id: 1,
        context_tokens: 90_000,
        context_window_tokens: 100_000,
    });
    active.input_tokens = 10_100;
    active.output_tokens = 10_100;
    let active_frame = render_to_string(&active, 40, 24);
    let active_footer = active_frame.lines().rev().take(2).collect::<Vec<_>>();
    assert!(
        active_footer.iter().any(|line| line.contains("ctx ~90%")),
        "{active_footer:?}"
    );
    assert!(active_footer
        .iter()
        .all(|line| !line.contains('↑') && !line.contains('↓')));
    assert!(active_footer.iter().all(|line| line.chars().count() <= 40));

    let mut pinned = active;
    pinned.working = false;
    pinned.activity = None;
    pinned.scroll.mode = FollowMode::Top;
    pinned.scroll.unseen = u32::MAX;
    let pinned_frame = render_to_string(&pinned, 40, 24);
    let pinned_footer = pinned_frame.lines().rev().take(2).collect::<Vec<_>>();
    assert!(
        pinned_footer.iter().any(|line| line.contains("End")),
        "{pinned_footer:?}"
    );
    assert!(pinned_footer
        .iter()
        .all(|line| !line.contains('↑') && !line.contains('↓')));
    assert!(pinned_footer.iter().all(|line| line.chars().count() <= 40));
}

#[test]
fn working_activity_is_immediately_above_inset_composer() {
    let mut state = AppState::new();
    state.authenticated = true;
    reduce(
        &mut state,
        slim_tui::reducer::Action::UiEventReceived(UiEvent::UserMessageAdded {
            text: "prompt".into(),
        }),
    );
    state.working = true;
    let frame = render_to_string(&state, 100, 24);
    let lines = frame.lines().collect::<Vec<_>>();
    let top_index = lines
        .iter()
        .position(|line| line.contains('╭'))
        .expect("top border");
    assert!(top_index > 0);
    assert!(lines[top_index - 1].contains("Working"));
    let footer = lines.last().expect("footer");
    assert!(
        !footer.contains("Working"),
        "activity label must not duplicate"
    );
    assert!(footer.contains("Ctrl+C cancel"));
}

#[test]
fn constrained_working_state_moves_from_activity_to_footer() {
    let mut state = AppState::new();
    state.working = true;
    state.todo_dock_open = true;
    state.todo_items = vec![
        TodoItemView {
            title: "index files".into(),
            status: TodoItemStatus::InProgress,
        },
        TodoItemView {
            title: "run checks".into(),
            status: TodoItemStatus::Pending,
        },
    ];

    let regions = plan(40, 8, 2, true);
    assert_eq!(regions.activity_rail.height, 0, "§14.4 degrades rail first");
    let frame = render_to_string(&state, 40, 8);
    let footer = frame.lines().last().expect("footer");
    assert!(
        footer.contains("Working"),
        "active state migrates to footer"
    );
    assert!(
        footer.contains("Ctrl+C cancel"),
        "the actionable cancellation shortcut remains visible: {footer}"
    );
}

#[test]
fn narrow_signed_out_footer_preserves_login_and_metrics() {
    let mut state = AppState::new();
    state.composer.insert_text("draft");
    let frame = render_to_string(&state, 40, 8);
    let footer = frame.lines().last().expect("footer");
    assert!(footer.contains("/login"), "critical auth action: {footer}");
    assert!(
        !footer.contains("ctx --"),
        "unknown context stays hidden: {footer}"
    );
}

#[test]
fn empty_session_footer_hides_unknown_context_and_zero_usage() {
    let state = AppState::new();
    let frame = render_to_string(&state, 80, 24);
    let footer = frame.lines().last().unwrap_or_default();
    assert!(footer.contains("signed out · /login"), "{footer}");
    assert!(!footer.contains("ctx --"), "{footer}");
    assert!(!footer.contains("↑0 ↓0"), "{footer}");
}

#[test]
fn pending_images_render_as_honest_composer_chips() {
    let mut state = AppState::new();
    state.apply_event(UiEvent::AttachmentsChanged {
        labels: vec!["screen.png".into()],
    });
    let frame = render_to_string(&state, 80, 24);
    assert!(frame.contains("image · screen.png"), "{frame}");
    assert!(frame.contains("1 image"), "{frame}");
}

#[test]
fn supported_sizes_keep_three_row_composer_without_op_gap() {
    for (width, height) in [(40, 8), (48, 12), (79, 15), (80, 16), (120, 24)] {
        let regions = plan(width, height, 0, false);
        assert_eq!(regions.composer.height, 3, "size={width}x{height}");
        assert_eq!(regions.op_divider.height, 0, "size={width}x{height}");
        assert_eq!(
            regions.operational.y,
            regions.composer.y + regions.composer.height,
            "size={width}x{height}"
        );
    }
}

#[test]
fn working_activity_sits_immediately_above_composer() {
    let regions = plan(100, 24, 0, true);
    assert_eq!(regions.activity_rail.height, 1);
    assert_eq!(
        regions.activity_rail.y + regions.activity_rail.height,
        regions.composer.y
    );
}

#[test]
fn emergency_layout_keeps_one_functional_row_each() {
    let state = todo_state();
    let frame = render_to_string(&state, 39, 7);
    assert!(!frame.is_empty());
    // A complete render without panic is the emergency-layout contract (§14.5).
}

#[test]
fn opencode_api_key_overlay_masks_secret() {
    let mut key = SensitiveText::default();
    assert!(key.push_str_bounded("fixture-opencode-secret", 4_096));
    let mut state = AppState::new();
    state.login_overlay = Some(LoginOverlay {
        selected: 2,
        stage: LoginStage::ApiKey(key),
        in_progress: false,
        progress: None,
        auth_url: None,
        user_code: None,
    });

    let frame = render_to_string(&state, 80, 24);

    assert!(frame.contains("OpenCode Go API key"), "{frame}");
    assert!(!frame.contains("fixture-opencode-secret"));
    assert!(frame.contains("••••"));
}

#[test]
fn speed_accounting_is_preserved_without_footer_telemetry() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::run_started(1));
    state.apply_event(UiEvent::UsageEstimateForRun {
        run_id: 1,
        request_id: 1,
        context_tokens: 500,
        context_window_tokens: 32_000,
    });
    state.provider_timings.first_semantic_ms = Some(100);
    state.apply_event(UiEvent::AssistantDelta {
        text: "Hello world!".into(),
    });
    // Delivery/rendering time must not replace the provider's two seconds.
    state.clock.elapsed_ms = 100_000;
    state.apply_event(UiEvent::Usage {
        input_tokens: 500,
        output_tokens: 170,
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::RequestCompleted {
        provider_latency_ms: 2_100,
    });

    assert_eq!(state.last_tok_per_sec, Some(850));
    assert!(!state.last_tok_per_sec_estimated);

    let wide = render_to_string(&state, 100, 24);
    let footer = wide.lines().rev().take(2).collect::<Vec<_>>();
    assert!(
        footer.iter().any(|line| line.contains("ctx")),
        "context belongs in the footer: {footer:?}"
    );
    assert!(
        !footer
            .iter()
            .any(|line| line.contains('↑') || line.contains('↓')),
        "totals stay out of footer: {footer:?}"
    );

    let narrow = render_to_string(&state, 60, 24);
    let narrow_footer = narrow.lines().last().expect("narrow footer");
    assert!(
        !narrow_footer.contains("t/s") && !narrow_footer.contains("tok/s"),
        "narrow footer omits speed: {narrow_footer}"
    );

    // Next provider request in the same run, with tool-only output.
    state.apply_event(UiEvent::UsageEstimateForRun {
        run_id: 1,
        request_id: 2,
        context_tokens: 600,
        context_window_tokens: 32_000,
    });
    assert_eq!(state.last_tok_per_sec, None);
    state.provider_timings.first_semantic_ms = Some(100);
    state.apply_event(UiEvent::Usage {
        input_tokens: 600,
        output_tokens: 200,
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::RequestCompleted {
        provider_latency_ms: 10_100,
    });
    assert_eq!(state.last_tok_per_sec, Some(200));
}

#[test]
fn estimated_speed_remains_in_diagnostics_only() {
    let mut state = AppState::new();
    state.authenticated = true;
    state.apply_event(UiEvent::run_started(1));
    state.provider_timings.first_semantic_ms = Some(100);
    state.apply_event(UiEvent::AssistantDelta {
        text: "a".repeat(400),
    });
    state.apply_event(UiEvent::AssistantEnded);
    state.apply_event(UiEvent::RequestCompleted {
        provider_latency_ms: 2_100,
    });
    assert!(state.last_tok_per_sec_estimated);
    let frame = render_to_string(&state, 100, 24);
    let footer = frame.lines().last().unwrap();
    assert!(!footer.contains('~'), "{frame}");
    assert!(!footer.contains("tok/s"), "{footer}");
    assert!(!slim_tui::view_model::status_line(&state, false).contains('~'));
    state.last_tok_per_sec = Some(500);
    state.inspector.active = Some(slim_tui::inspector::InspectorKind::Diagnostics);
    let details = render_to_string(&state, 140, 30);
    assert!(
        !details.lines().last().unwrap_or_default().contains("tok/s"),
        "throughput stays off the footer: {details}"
    );

    state.apply_event(UiEvent::RequestCompleted {
        provider_latency_ms: 100,
    });
    assert_eq!(state.last_tok_per_sec, None);
}

#[test]
fn completed_todos_collapse_and_can_be_expanded_on_demand() {
    let mut state = todo_state();
    state.apply_event(UiEvent::TodoChanged {
        items: state
            .todo_items
            .iter()
            .cloned()
            .map(|mut item| {
                item.status = TodoItemStatus::Completed;
                item
            })
            .collect(),
    });
    assert!(!state.todo_dock_open);
    let compact = render_to_string(&state, 140, 30);
    assert!(compact.contains("TODO 3/3"), "{compact}");
    assert!(!compact.contains("mapear fluxo"), "{compact}");
    reduce(&mut state, slim_tui::reducer::Action::ToggleTodoDock);
    let expanded = render_to_string(&state, 140, 30);
    assert!(expanded.contains("mapear fluxo"), "{expanded}");
    assert!(expanded.contains("revisar diff"), "{expanded}");
}
