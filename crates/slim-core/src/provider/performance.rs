//! Manual, network-free measurements of the production request preparation path.
use super::*;
use std::hint::black_box;

fn report(label: &str, samples: &mut [f64]) {
    samples.sort_by(f64::total_cmp);
    eprintln!(
        "{label}: n={} median_ms={:.3} min_ms={:.3} max_ms={:.3}",
        samples.len(),
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1]
    );
}

#[test]
#[ignore = "manual release performance measurement; no network"]
fn request_preparation_costs() {
    let adapter = OpenAiCodexAdapter::new(ProviderConfig::openai_codex(
        "http://127.0.0.1:1",
        "gpt-5.3-codex",
        "fixture-token",
        "fixture-account",
    ))
    .unwrap();
    let tools =
        crate::tools::ToolRegistry::default().definitions_for_mode(crate::OperatingMode::Auto);
    for (repeats, expected_body_hash) in [
        (16, 0x346f5b7194a27a82),
        (256, 0x483571a88baadf02),
        (1024, 0x5c482248d5598b02),
    ] {
        let messages: Vec<_> = (0..64)
            .map(|i| {
                let text = format!(
                    "{i}: {}",
                    "source: ação 日本語 \\\"field\\\"\n".repeat(repeats)
                );
                if i % 2 == 0 {
                    ProviderMessage::user(text)
                } else {
                    ProviderMessage::assistant(text, vec![])
                }
            })
            .collect();
        let prepared = adapter
            .prepare_messages_request_with_tools_checked(&messages, &tools)
            .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        // Captured before the accounting/affinity optimization: the full wire
        // request, including its native prompt key, must remain identical.
        assert_eq!(fnv1a64(&prepared.body), expected_body_hash);
        eprintln!(
            "request_bytes={} body_hash={:016x} components={:?} prefixes={:?}",
            prepared.body.len(),
            fnv1a64(&prepared.body),
            prepared.components,
            prepared.stable_prefixes
        );
        let mut complete = Vec::new();
        let mut affinity = Vec::new();
        let mut fingerprints = Vec::new();
        let mut components = Vec::new();
        for _ in 0..11 {
            let start = Instant::now();
            black_box(
                adapter
                    .prepare_messages_request_with_tools_checked(&messages, &tools)
                    .unwrap(),
            );
            complete.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            black_box(provider_native_prompt_cache_key(
                adapter.wire_kind(),
                adapter.model(),
                &body,
            ));
            affinity.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            black_box(provider_request_fingerprints(&body));
            fingerprints.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = Instant::now();
            black_box(provider_request_components(&body));
            components.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        report("prepare_complete", &mut complete);
        report("native_affinity", &mut affinity);
        report("fingerprints", &mut fingerprints);
        report("components", &mut components);
    }
}
