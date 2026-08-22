use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct LoopGuard {
    failed_calls: HashSet<String>,
}

impl LoopGuard {
    pub fn accept(&mut self, tool: &str, arguments: &str, error: &str) -> bool {
        let fingerprint = format!("{tool}\n{arguments}\n{error}");
        self.failed_calls.insert(fingerprint)
    }
}
