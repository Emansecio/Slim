/// Caller-supplied prices, in microdollars per million tokens. Unknown cache
/// prices must not be substituted with zero or treated as a measured saving.
#[derive(Clone, Copy, Debug)]
pub struct CompactionPricing {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Default)]
pub(super) struct JevEconomy {
    observations: std::collections::VecDeque<(u64, u64)>,
}

impl JevEconomy {
    pub(super) fn observe(&mut self, stats: &crate::context::JevPruneStats) {
        let (Some(input), Some(saved)) = (stats.input_tokens, stats.effective_saved_tokens) else {
            return;
        };
        if input == 0 || stats.usage_unknown {
            return;
        }
        if self.observations.len() == 4 {
            self.observations.pop_front();
        }
        self.observations.push_back((input, saved));
    }

    pub(super) fn expected_savings(&self, input: u64, maximum: u64) -> u64 {
        if self.observations.is_empty() {
            return maximum;
        }
        let (seen_input, seen_saved) = self
            .observations
            .iter()
            .fold((0u128, 0u128), |(i, s), &(input, saved)| {
                (i + u128::from(input), s + u128::from(saved))
            });
        // A small exploration allowance lets a materially larger opportunity
        // recover after unproductive attempts, without retrying the same cost.
        let learned = (u128::from(input).saturating_mul(seen_saved) / seen_input)
            .min(u128::from(u64::MAX)) as u64;
        maximum.min(learned.max(maximum / 10))
    }
}

impl CompactionPricing {
    pub(super) fn can_pay(
        self,
        before: u64,
        after: u64,
        input: u64,
        output: u64,
        turns: u8,
    ) -> bool {
        // Favor keeping the existing cache; charge the new prefix at the more
        // expensive write/uncached rate. Common units cancel, without rounding.
        let read = self.cache_read.min(self.input);
        let savings = u128::from(before.saturating_sub(after))
            .saturating_mul(u128::from(read))
            .saturating_mul(u128::from(turns));
        let cost = (u128::from(input) * u128::from(self.input))
            .saturating_add(u128::from(output) * u128::from(self.output))
            .saturating_add(
                u128::from(after) * u128::from(self.cache_write.max(self.input) - read),
            );
        savings > cost.saturating_add(cost / 4)
    }
}

// A turn budget is only a ceiling. Extend the two-turn fallback only with
// observed task throughput and actionable work, bounded to eight predictions.
pub(super) fn future_turns(remaining: usize, turns: usize, completed: usize, open: usize) -> u8 {
    let forecast = if completed > 0 && open > 0 {
        turns.div_ceil(completed).saturating_mul(open).clamp(1, 8)
    } else if completed > 0 && open == 0 {
        1
    } else {
        2
    };
    remaining.min(forecast) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jev_forecast_learns_only_complete_metered_attempts_and_can_recover() {
        let mut economy = JevEconomy::default();
        assert_eq!(economy.expected_savings(1_000, 10_000), 10_000);
        economy.observe(&crate::context::JevPruneStats {
            input_tokens: Some(1_000),
            effective_saved_tokens: Some(2_000),
            ..Default::default()
        });
        assert_eq!(economy.expected_savings(1_000, 10_000), 2_000);
        economy.observe(&crate::context::JevPruneStats {
            input_tokens: Some(1_000),
            effective_saved_tokens: Some(0),
            usage_unknown: true,
            ..Default::default()
        });
        economy.observe(&crate::context::JevPruneStats {
            input_tokens: Some(1_000),
            ..Default::default()
        });
        assert_eq!(economy.expected_savings(1_000, 10_000), 2_000);
        for _ in 0..4 {
            economy.observe(&crate::context::JevPruneStats {
                input_tokens: Some(1_000),
                effective_saved_tokens: Some(0),
                ..Default::default()
            });
        }
        assert_eq!(economy.expected_savings(1_000, 10_000), 1_000);
        assert_eq!(economy.expected_savings(1_000, 100_000), 10_000);
        assert_eq!(economy.expected_savings(1_000, 0), 0);
    }

    #[test]
    fn cheap_cache_and_prefix_rebuild_can_reverse_token_savings() {
        let normal = CompactionPricing {
            input: 1000,
            output: 1000,
            cache_read: 1000,
            cache_write: 1000,
        };
        assert!(normal.can_pay(100_000, 20_000, 30_000, 2048, 2));
        assert!(!CompactionPricing {
            cache_read: 10,
            ..normal
        }
        .can_pay(100_000, 20_000, 30_000, 2048, 2));
        assert!(!CompactionPricing {
            cache_write: 20_000,
            ..normal
        }
        .can_pay(100_000, 20_000, 30_000, 2048, 2));
        assert!(!normal.can_pay(10, 20, 1, 1, 8));
    }

    #[test]
    fn horizon_uses_observed_work_and_never_exceeds_remaining_budget() {
        assert_eq!(future_turns(128, 0, 0, 10), 2);
        assert_eq!(future_turns(128, 6, 2, 3), 8);
        assert_eq!(future_turns(3, 6, 2, 3), 3);
        assert_eq!(future_turns(128, 6, 2, 0), 1);
        assert_eq!(future_turns(0, 6, 2, 3), 0);
    }
}
