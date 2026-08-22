//! Measures the per-turn tool-definition setup used by the agent loop.

use std::hint::black_box;
use std::time::Instant;

use slim_core::tools::ToolRegistry;
use slim_core::OperatingMode;

const ITERATIONS: u32 = 20_000;
const SAMPLES: usize = 11;

fn per_turn(registry: &ToolRegistry) -> (u128, usize) {
    let start = Instant::now();
    let mut bytes = 0;
    for _ in 0..ITERATIONS {
        let tools = registry.definitions_for_mode(OperatingMode::Auto);
        bytes = serde_json::to_vec(&tools).expect("serialize tools").len();
        black_box((&tools, bytes));
    }
    (start.elapsed().as_nanos(), bytes)
}

fn cached(registry: &ToolRegistry) -> (u128, usize) {
    let start = Instant::now();
    let tools = registry.definitions_for_mode(OperatingMode::Auto);
    let bytes = serde_json::to_vec(&tools).expect("serialize tools").len();
    for _ in 0..ITERATIONS {
        black_box((&tools, bytes));
    }
    (start.elapsed().as_nanos(), bytes)
}

fn median_and_min(mut samples: Vec<u128>) -> (u128, u128) {
    samples.sort_unstable();
    (samples[samples.len() / 2], samples[0])
}

fn main() {
    let registry = ToolRegistry::default();
    let mut per_turn_samples = Vec::with_capacity(SAMPLES);
    let mut cached_samples = Vec::with_capacity(SAMPLES);
    let mut serialized_bytes = 0;

    black_box(per_turn(&registry));
    black_box(cached(&registry));

    for sample in 0..SAMPLES {
        if sample % 2 == 0 {
            let (elapsed, bytes) = per_turn(&registry);
            per_turn_samples.push(elapsed);
            serialized_bytes = bytes;

            let (elapsed, cached_bytes) = cached(&registry);
            assert_eq!(bytes, cached_bytes);
            cached_samples.push(elapsed);
        } else {
            let (elapsed, cached_bytes) = cached(&registry);
            cached_samples.push(elapsed);

            let (elapsed, bytes) = per_turn(&registry);
            assert_eq!(bytes, cached_bytes);
            per_turn_samples.push(elapsed);
            serialized_bytes = bytes;
        }
    }

    let (per_turn_median, per_turn_min) = median_and_min(per_turn_samples);
    let (cached_median, cached_min) = median_and_min(cached_samples);
    let iterations = f64::from(ITERATIONS);
    let per_turn_ns = per_turn_median as f64 / iterations;
    let cached_ns = cached_median as f64 / iterations;

    println!(
        "mode=auto tools={} serialized_bytes={} iterations={} samples={}",
        registry.names_for_mode(OperatingMode::Auto).len(),
        serialized_bytes,
        ITERATIONS,
        SAMPLES
    );
    println!(
        "per_turn_median_total_ns={per_turn_median} per_turn_min_total_ns={per_turn_min} \
         cached_median_total_ns={cached_median} cached_min_total_ns={cached_min}"
    );
    println!(
        "per_turn_median_ns_per_iteration={per_turn_ns:.3} \
         cached_median_ns_per_iteration={cached_ns:.3} speedup_x={:.1}",
        per_turn_ns / cached_ns
    );
}
