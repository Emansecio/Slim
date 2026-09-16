use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use slim_tui::api::UiCommand;
use slim_tui::app::AppState;
use slim_tui::composer::{Composer, ComposerError, MAX_DRAFT_CHARS};
use slim_tui::input::{
    classify_enter, is_submit, normalize, BracketedPasteDecoder, DecodedEvent, DecodedInput,
    EnterIntent, PasteStreamDecoder,
};
use slim_tui::reducer::{reduce, Action, Effect};

fn press(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn marker_events(payload: &str) -> Vec<Event> {
    "\u{1b}[200~"
        .chars()
        .chain(payload.chars())
        .chain("\u{1b}[201~".chars())
        .map(|character| {
            press(match character {
                '\u{1b}' => KeyCode::Esc,
                '\r' | '\n' => KeyCode::Enter,
                other => KeyCode::Char(other),
            })
        })
        .collect()
}

fn collect(decoder: &mut PasteStreamDecoder, events: Vec<Event>) -> Vec<DecodedEvent> {
    let last = events.len().saturating_sub(1);
    let mut decoded = Vec::new();
    for (index, event) in events.into_iter().enumerate() {
        decoded.extend(decoder.feed(event, index < last));
    }
    decoded.extend(decoder.end_of_input());
    decoded
}

fn key_event(decoded: &DecodedEvent) -> Option<KeyEvent> {
    match decoded {
        DecodedEvent::Event(Event::Key(key)) => Some(*key),
        _ => None,
    }
}

#[test]
fn paste_is_atomic_and_submits_full_payload() {
    let mut composer = Composer::default();
    composer.insert_text("before ");
    composer.paste("line one\nline two");
    assert!(composer.display().contains("[Pasted Content 0 17 chars]"));
    assert_eq!(composer.payload(), "before line one\nline two");
}

#[test]
fn oversized_draft_is_rejected_and_paste_segment_can_be_removed_atomically() {
    let mut composer = Composer::default();
    assert_eq!(
        composer.try_paste("x".repeat(MAX_DRAFT_CHARS + 1)),
        Err(ComposerError::DraftTooLarge)
    );
    composer.try_paste("segment").expect("paste");
    assert_eq!(composer.payload(), "segment");
    assert!(composer.remove_last_element().is_some());
    assert!(composer.payload().is_empty());
}

#[test]
fn sequential_composer_input_stays_within_a_linear_latency_budget() {
    let mut composer = Composer::default();
    let started = std::time::Instant::now();
    for _ in 0..50_000 {
        composer.try_insert_text("x").expect("within limit");
    }
    let elapsed = started.elapsed();

    assert_eq!(composer.payload().len(), 50_000);
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "50k sequential inserts took {elapsed:?}"
    );
}

#[test]
fn backspace_removes_last_grapheme_and_whole_trailing_paste() {
    let mut composer = Composer::default();
    composer.insert_text("run /logout ");
    assert!(composer.backspace());
    assert_eq!(composer.payload(), "run /logout");

    let mut emoji = Composer::default();
    emoji.insert_text("ab\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F466}");
    assert!(emoji.backspace());
    assert_eq!(emoji.payload(), "ab", "ZWJ family is one grapheme cluster");

    let mut pasted = Composer::default();
    pasted.insert_text("kept ");
    pasted.paste("pasted\ntext");
    assert!(pasted.backspace());
    assert_eq!(
        pasted.payload(),
        "kept ",
        "trailing paste segment is removed atomically"
    );

    let mut empty = Composer::default();
    assert!(!empty.backspace());
}

#[test]
fn composer_edits_at_a_real_grapheme_safe_cursor() {
    let mut composer = Composer::default();
    composer.insert_text("ac");
    assert!(composer.move_left());
    composer.insert_text("b");
    assert_eq!(composer.payload(), "abc");
    assert_eq!(composer.cursor(), 2);

    assert!(composer.move_left());
    assert!(composer.delete());
    assert_eq!(composer.payload(), "ac");

    let mut emoji = Composer::default();
    emoji.insert_text("a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F466}b");
    assert!(emoji.move_left());
    assert!(emoji.move_left());
    assert_eq!(emoji.cursor(), 1, "ZWJ family is crossed as one grapheme");
}

#[test]
fn paste_remains_atomic_when_editing_away_from_the_end() {
    let mut composer = Composer::default();
    composer.insert_text("before ");
    composer.paste("one\ntwo");
    composer.insert_text(" after");

    for _ in 0.." after".chars().count() {
        assert!(composer.move_left());
    }
    assert!(composer.backspace());
    assert_eq!(composer.payload(), "before  after");
}

#[test]
fn home_end_and_snapshot_follow_the_cursor_line() {
    let mut composer = Composer::default();
    composer.insert_text("alpha\nbravo\ncharlie");
    assert!(composer.move_home());
    composer.insert_text(">");
    assert_eq!(composer.payload(), "alpha\nbravo\n>charlie");
    assert!(composer.move_end());
    let snapshot = composer.display_snapshot(40);
    assert_eq!(snapshot.cursor_line, 2);
    assert_eq!(snapshot.total_lines, 3);
    assert_eq!(snapshot.cursor_cell, 8);
}

#[test]
fn clear_removes_submitted_text_and_paste_segments() {
    let mut composer = Composer::default();
    composer.insert_text("prompt");
    composer.paste("pasted\ntext");
    composer.clear();
    assert!(composer.payload().is_empty());
}

#[test]
fn raw_bracketed_paste_decoder_preserves_fragmented_multiline_payload() {
    let mut decoder = BracketedPasteDecoder::default();
    let mut decoded = decoder.feed(b"prefix\x1b[20");
    assert!(decoder.feed(b"0~line one\nline two\x1b[201").is_empty());
    decoded.extend(decoder.feed(b"~suffix"));
    assert_eq!(
        decoded,
        vec![
            DecodedInput::Text("prefix".into()),
            DecodedInput::Paste("line one\nline two".into()),
            DecodedInput::Text("suffix".into()),
        ]
    );
}

#[test]
fn plain_enter_submits_and_control_enter_is_newline_fallback() {
    let plain = normalize(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(is_submit(plain));
    let key = normalize(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
    assert_eq!(classify_enter(key, false), EnterIntent::Newline);
}

#[test]
fn modifier_matrix_and_release_events_are_normalized_without_duplicate_submit() {
    let shift = normalize(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    let alt = normalize(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
    let release = normalize(KeyEvent::new_with_kind(
        KeyCode::Enter,
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    assert!(!is_submit(shift));
    assert_eq!(classify_enter(alt, true), EnterIntent::Steer);
    assert!(!is_submit(release));
}

#[test]
fn marker_key_sequence_decodes_one_atomic_multiline_paste() {
    let mut decoder = PasteStreamDecoder::default();

    let decoded = collect(&mut decoder, marker_events("line one\nline two"));

    assert_eq!(
        decoded,
        vec![DecodedEvent::Paste("line one\nline two".into())]
    );
}

#[test]
fn marker_paste_reaching_the_composer_never_sends_a_prompt() {
    let mut decoder = PasteStreamDecoder::default();
    let mut state = AppState::new();

    for decoded in collect(
        &mut decoder,
        marker_events("first line\nsecond line\nthird"),
    ) {
        let action = match decoded {
            DecodedEvent::Paste(payload) => Some(Action::Paste(payload)),
            DecodedEvent::Event(Event::Key(key)) => Some(Action::Key(key)),
            DecodedEvent::Event(_) => None,
        };
        if let Some(action) = action {
            for effect in reduce(&mut state, action) {
                assert!(
                    !matches!(effect, Effect::Send(UiCommand::SendPrompt(_))),
                    "paste must never submit"
                );
            }
        }
    }

    assert_eq!(state.composer.payload(), "first line\nsecond line\nthird");
}

#[test]
fn unmarked_paste_burst_turns_newlines_into_draft_text_not_submits() {
    let mut decoder = PasteStreamDecoder::default();
    let events = "line one\nline two"
        .chars()
        .map(|character| {
            press(match character {
                '\n' => KeyCode::Enter,
                other => KeyCode::Char(other),
            })
        })
        .collect();

    let decoded = collect(&mut decoder, events);

    assert!(
        decoded
            .iter()
            .all(|decoded| key_event(decoded).is_none_or(|key| key.code != KeyCode::Enter)),
        "burst Enter must not survive as a submit: {decoded:?}"
    );
    let text: String = decoded
        .iter()
        .filter_map(|decoded| match key_event(decoded)?.code {
            KeyCode::Char(character) => Some(character),
            _ => None,
        })
        .collect();
    assert_eq!(text, "line one\nline two");
}

#[test]
fn a_lone_enter_still_submits_and_lone_esc_still_fires() {
    let mut decoder = PasteStreamDecoder::default();
    assert_eq!(
        collect(&mut decoder, vec![press(KeyCode::Enter)]),
        vec![DecodedEvent::Event(press(KeyCode::Enter))]
    );
    assert_eq!(
        collect(&mut decoder, vec![press(KeyCode::Esc)]),
        vec![DecodedEvent::Event(press(KeyCode::Esc))]
    );
}

#[test]
fn a_marker_prefix_flushed_by_input_end_stays_literal_text() {
    let mut decoder = PasteStreamDecoder::default();

    let decoded = collect(
        &mut decoder,
        vec![
            press(KeyCode::Esc),
            press(KeyCode::Char('[')),
            press(KeyCode::Char('2')),
        ],
    );

    assert_eq!(
        decoded,
        vec![
            DecodedEvent::Event(press(KeyCode::Esc)),
            DecodedEvent::Event(press(KeyCode::Char('['))),
            DecodedEvent::Event(press(KeyCode::Char('2'))),
        ]
    );
}

#[test]
fn pasted_text_containing_a_false_end_marker_survives_intact() {
    let mut decoder = PasteStreamDecoder::default();

    let decoded = collect(&mut decoder, marker_events("a\u{1b}[20Xb"));

    assert_eq!(decoded, vec![DecodedEvent::Paste("a\u{1b}[20Xb".into())]);
}

#[test]
fn paste_landing_while_idle_leaves_the_draft_ready_for_a_manual_submit() {
    let mut decoder = PasteStreamDecoder::default();
    let mut state = AppState::new();
    state.authenticated = true;

    for decoded in collect(&mut decoder, marker_events("todo\nlist")) {
        if let DecodedEvent::Paste(payload) = decoded {
            reduce(&mut state, Action::Paste(payload));
        }
    }
    assert_eq!(state.composer.payload(), "todo\nlist");

    let effects = reduce(
        &mut state,
        Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
    );
    assert!(effects
        .iter()
        .any(|effect| matches!(effect, Effect::Send(UiCommand::SendPrompt(_)))));
}
