use slim_core::SessionSnapshot;

#[derive(Clone, Debug, Default)]
pub struct FakeProvider;

impl FakeProvider {
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot::new("fake-session", 1)
    }
}
