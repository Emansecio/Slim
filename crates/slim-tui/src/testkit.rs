use crate::view_model::Frame;

#[derive(Default)]
pub struct MemorySurface {
    pub frames: Vec<Frame>,
}

impl MemorySurface {
    pub fn draw(&mut self, frame: Frame) {
        self.frames.push(frame);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FakeClock {
    pub ticks: u64,
}
