use slim_core::AppHandle;
use slim_core::{EventKind, SessionEvent};
use slim_tui::api::UiEvent;
use slim_tui::app::AppState;
use slim_tui::testkit::render_terminal_text;

#[test]
fn projects_core_events_into_the_real_tui_renderer() {
    let mut app = AppHandle::fake();
    app.push_event(SessionEvent::new(
        1,
        EventKind::SessionStarted {
            session_id: "smoke-session".into(),
        },
    ))
    .expect("session event");
    app.push_event(SessionEvent::new(
        2,
        EventKind::AssistantTextDelta {
            text: "hello".into(),
        },
    ))
    .expect("event");
    let mut state = AppState::new();
    for event in app.drain_events() {
        state.apply_event(UiEvent::from_core(event).expect("mapped core event"));
    }
    assert!(render_terminal_text(&state, 80, 24).contains("hello"));
}
