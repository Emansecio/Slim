//! Deterministic scaling gate of Pi's cut point and compaction preparation
//! for long conversations.

use std::hint::black_box;
use std::time::Instant;

use slim_core::context::{find_cut_point, prepare_compaction, CompactionSettings, ContextUsage};
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

fn p95_ms(mut run: impl FnMut()) -> f64 {
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..8 {
        run();
    }
    for _ in 0..SAMPLES {
        let started = Instant::now();
        run();
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    samples.sort_by(f64::total_cmp);
    samples[SAMPLES * 95 / 100]
}

fn measure(label: &str, mut run: impl FnMut(&[ProviderMessage])) {
    let ten_thousand = history(10_000);
    let twenty_thousand = history(20_000);
    let p95_10k = p95_ms(|| run(&ten_thousand));
    let p95_20k = p95_ms(|| run(&twenty_thousand));
    let ratio = p95_20k / p95_10k.max(f64::EPSILON);

    println!(
        "{label}_10k_p95_ms={p95_10k:.3} {label}_20k_p95_ms={p95_20k:.3} scaling_x={ratio:.2}"
    );
    assert!(p95_20k < 50.0, "20k {label} p95 exceeded 50 ms");
    assert!(ratio < 3.0, "10k to 20k {label} scaling exceeded 3x");
}

fn main() {
    let settings = CompactionSettings::default();
    measure("cut_point", |messages| {
        black_box(find_cut_point(
            black_box(messages),
            0,
            messages.len(),
            settings.keep_recent_tokens,
        ));
    });
    measure("preparation", |messages| {
        black_box(
            prepare_compaction(black_box(messages), &settings, ContextUsage::default())
                .expect("preparation"),
        );
    });
}
