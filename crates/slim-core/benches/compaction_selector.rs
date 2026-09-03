//! Deterministic selector scaling gate for long conversations.

use std::hint::black_box;
use std::time::Instant;

use slim_core::context::{select_compaction_history, CompactionPolicy};
use slim_core::provider::ProviderMessage;

const SAMPLES: usize = 80;

fn history(len: usize) -> Vec<ProviderMessage> {
    (0..len)
        .map(|index| {
            if index % 2 == 0 {
                ProviderMessage::user(format!("request-{index}: {}", "context ".repeat(8)))
            } else {
                ProviderMessage::assistant(
                    format!("answer-{index}: {}", "result ".repeat(8)),
                    Vec::new(),
                )
            }
        })
        .collect()
}

fn p95_ms(messages: &[ProviderMessage], policy: &CompactionPolicy) -> f64 {
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..8 {
        black_box(select_compaction_history(messages, policy).expect("warm selection"));
    }
    for _ in 0..SAMPLES {
        let started = Instant::now();
        black_box(select_compaction_history(messages, policy).expect("selection"));
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    samples.sort_by(f64::total_cmp);
    samples[SAMPLES * 95 / 100]
}

fn main() {
    let policy = CompactionPolicy::default();
    let ten_thousand = history(10_000);
    let twenty_thousand = history(20_000);
    let p95_10k = p95_ms(&ten_thousand, &policy);
    let p95_20k = p95_ms(&twenty_thousand, &policy);
    let ratio = p95_20k / p95_10k.max(f64::EPSILON);

    println!(
        "selector_10k_p95_ms={p95_10k:.3} selector_20k_p95_ms={p95_20k:.3} scaling_x={ratio:.2}"
    );
    assert!(p95_20k < 50.0, "20k selector p95 exceeded 50 ms");
    assert!(ratio < 3.0, "10k to 20k selector scaling exceeded 3x");
}
