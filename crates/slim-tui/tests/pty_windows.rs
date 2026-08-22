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
