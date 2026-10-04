use super::{cost_summary_for_usage, UsagePricing};
use slim_core::{ProviderPricing, RequestKind, RequestUsage, UsageTotals};

#[test]
fn costs_separate_failed_compaction_cancelled_and_validated_work() {
    let pricing = UsagePricing {
        provider: ProviderPricing {
            input_micros_per_million: 1_000_000,
            output_micros_per_million: 2_000_000,
        },
        cache_write_micros_per_million: Some(3_000_000),
        cache_read_micros_per_million: Some(500_000),
    };
    let usage = UsageTotals {
        validated_completion: true,
        requests: vec![
            RequestUsage {
                request_kind: RequestKind::ProviderTurn,
                uncached_input_tokens: 10,
                cache_write_tokens: 2,
                cache_read_tokens: 4,
                output_tokens: 3,
                ..RequestUsage::default()
            },
            RequestUsage {
                request_kind: RequestKind::ProviderTurn,
                uncached_input_tokens: 5,
                output_tokens: 1,
                failed: true,
                ..RequestUsage::default()
            },
            RequestUsage {
                request_kind: RequestKind::Compaction,
                uncached_input_tokens: 4,
                output_tokens: 2,
                ..RequestUsage::default()
            },
        ],
        ..UsageTotals::default()
    };

    let costs = cost_summary_for_usage(&usage, Some(pricing));
    assert_eq!(costs.total_micros, Some(39));
    assert_eq!(costs.cost_per_validated_completion_micros, Some(39));
    assert_eq!(costs.failed_attempts_micros, Some(7));
    assert_eq!(costs.compaction_micros, Some(8));

    let cancelled = UsageTotals {
        requests: vec![RequestUsage {
            request_kind: RequestKind::ProviderTurn,
            usage_unknown: true,
            cancelled: true,
            estimated_input_tokens: 6,
            ..RequestUsage::default()
        }],
        usage_unknown: true,
        ..UsageTotals::default()
    };
    let costs = cost_summary_for_usage(&cancelled, Some(pricing));
    assert_eq!(costs.total_micros, None);
    assert_eq!(costs.cancelled_estimated_micros, Some(6));

    let overflowed = UsageTotals {
        requests: usage.requests,
        overflowed: true,
        ..UsageTotals::default()
    };
    let costs = cost_summary_for_usage(&overflowed, Some(pricing));
    assert_eq!(costs.total_micros, None);
    assert_eq!(costs.failed_attempts_micros, None);
    assert_eq!(costs.compaction_micros, None);
}
#[test]
fn request_costs_round_after_aggregation() {
    let pricing = UsagePricing {
        provider: ProviderPricing {
            input_micros_per_million: 1_500_000,
            output_micros_per_million: 0,
        },
        cache_write_micros_per_million: Some(0),
        cache_read_micros_per_million: Some(0),
    };
    let usage = UsageTotals {
        requests: vec![
            RequestUsage {
                uncached_input_tokens: 1,
                ..RequestUsage::default()
            },
            RequestUsage {
                uncached_input_tokens: 1,
                ..RequestUsage::default()
            },
        ],
        ..UsageTotals::default()
    };

    let costs = cost_summary_for_usage(&usage, Some(pricing));

    assert_eq!(costs.total_micros, Some(3));
}

#[test]
fn cancelled_cost_combines_observed_usage_with_unknown_input_remainder() {
    let pricing = UsagePricing {
        provider: ProviderPricing {
            input_micros_per_million: 1_000_000,
            output_micros_per_million: 2_000_000,
        },
        cache_write_micros_per_million: Some(3_000_000),
        cache_read_micros_per_million: Some(500_000),
    };
    let usage = UsageTotals {
        requests: vec![RequestUsage {
            uncached_input_tokens: 2,
            cache_write_tokens: 1,
            cache_read_tokens: 3,
            output_tokens: 4,
            estimated_input_tokens: 10,
            usage_unknown: true,
            cancelled: true,
            ..RequestUsage::default()
        }],
        usage_unknown: true,
        ..UsageTotals::default()
    };

    let costs = cost_summary_for_usage(&usage, Some(pricing));

    assert_eq!(costs.cancelled_estimated_micros, Some(18));
}
