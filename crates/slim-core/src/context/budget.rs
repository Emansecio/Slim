#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextBudget {
    pub window_tokens: u64,
    pub used_tokens: u64,
    pub reserve_tokens: u64,
}

impl ContextBudget {
    pub fn new(window_tokens: u64, used_tokens: u64, reserve_tokens: u64) -> Self {
        Self {
            window_tokens,
            used_tokens,
            reserve_tokens,
        }
    }

    pub fn can_fit(self, additional_tokens: u64) -> bool {
        self.used_tokens.saturating_add(additional_tokens) <= self.window_tokens
    }
}
