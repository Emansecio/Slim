use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use slim_tui::composer::{Composer, ComposerError, MAX_DRAFT_CHARS};
use slim_tui::input::{
    classify_enter, is_submit, normalize, BracketedPasteDecoder, DecodedInput, EnterIntent,
};

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
