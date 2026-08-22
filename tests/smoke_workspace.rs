#[path = "support/fake_clock.rs"]
mod fake_clock;
#[path = "support/fake_provider.rs"]
mod fake_provider;

use fake_clock::FakeClock;
use fake_provider::FakeProvider;
use slim_cli::compose_app;
use slim_core::AppHandle;
use slim_core::{EventKind, SessionEvent};
use slim_tui::api::UiEvent;
use slim_tui::render_snapshot;

#[test]
fn composes_all_workspace_crates_with_a_fake_snapshot() {
    let mut clock = FakeClock::new();
    clock.tick();

    let provider = FakeProvider;
    let snapshot = provider.snapshot();
    let mut app: AppHandle = compose_app();
    app.apply_session_snapshot(snapshot.clone());

    assert_eq!(clock.ticks(), 1);
    assert_eq!(app.snapshot(), Some(&snapshot));
    assert_eq!(render_snapshot(&snapshot), "fake-session#1");

    app.push_event(SessionEvent::new(
        1,
        EventKind::AssistantTextDelta {
            text: "hello".into(),
        },
    ))
    .expect("event");
    let event = app.drain_events().pop().expect("drained event");
    assert_eq!(
        UiEvent::from_core(event),
        Some(UiEvent::AssistantDelta {
            text: "hello".into()
        })
    );
}
