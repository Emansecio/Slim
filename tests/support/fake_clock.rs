#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FakeClock {
    ticks: u64,
}

impl FakeClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tick(&mut self) {
        self.ticks += 1;
    }

    pub fn ticks(&self) -> u64 {
        self.ticks
    }
}
