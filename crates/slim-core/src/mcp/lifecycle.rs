#[derive(Debug, Default)]
pub struct McpLifecycle {
    running: bool,
}

impl McpLifecycle {
    pub fn new() -> Self {
        Self { running: true }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn cancel(&mut self) {
        self.running = false;
    }
}
