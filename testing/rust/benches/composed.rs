mod support;
use hdrhistogram::Histogram;
use serde_json::json;
use std::time::{Duration, Instant};
fn parameter(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |value| {
        value.parse().expect("positive integer benchmark parameter")
    })
}
fn main() {
    let concurrency = parameter("TEMBRA_BENCH_CONCURRENCY", 256);
    let samples = parameter("TEMBRA_BENCH_SAMPLES", 10);
    let rounds = parameter("TEMBRA_BENCH_ROUNDS", 8);
    let warmup = parameter("TEMBRA_BENCH_WARMUP", 2);
    assert!(
        (1..=4096).contains(&concurrency)
            && (2..=1000).contains(&samples)
            && (1..=1000).contains(&rounds)
            && warmup <= 1000
    );
    for sample in 0..samples + warmup {
        // Every sample starts an independent Host; warmup samples are explicitly discarded.
        let mut fixture = support::Fixture::new(16, true);
        let mut latency = Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3).unwrap();
        let mut wall = Duration::ZERO;
        for _ in 0..rounds {
            fixture.drain();
            let start = Instant::now();
            let elapsed = fixture.turns(concurrency);
            wall += start.elapsed();
            for elapsed in elapsed {
                latency
                    .record(elapsed.max(1))
                    .expect("turn exceeded 60-second histogram range");
            }
        }
        fixture.report("composed_sample");
        let observations = fixture.health();
        println!(
            "{}",
            json!({"workload":"four_calls_state16_registry_pending_v1", "sample":sample, "warmup":sample < warmup,
            "contract":fixture.contract.name(), "concurrency":concurrency, "observed_max_pending":fixture.maximum.get(), "turns":concurrency*rounds,
            "method_calls":concurrency*rounds*4, "wall_ns":wall.as_nanos().to_string(),
            "turns_per_second":(concurrency*rounds) as f64 / wall.as_secs_f64(),
            "latency_ns":{"p50":latency.value_at_quantile(0.50), "p95":latency.value_at_quantile(0.95), "p99":latency.value_at_quantile(0.99), "max":latency.max(), "samples":latency.len()},
            "latency_model":"closed_loop_individual_turns_including_suspension", "cpu_ns":null, "allocated_bytes":null, "observations":observations})
        );
    }
}
