use super::compact::CompactionPolicy;

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

    pub fn threshold_tokens(self) -> u64 {
        CompactionPolicy::default().hard_threshold_tokens(self.window_tokens)
    }

    pub fn should_compact(self) -> bool {
        CompactionPolicy::default().is_over_hard(
            self.used_tokens,
            self.window_tokens,
            self.reserve_tokens,
        )
    }

    pub fn can_fit(self, additional_tokens: u64) -> bool {
        self.used_tokens.saturating_add(additional_tokens) <= self.window_tokens
    }
}
