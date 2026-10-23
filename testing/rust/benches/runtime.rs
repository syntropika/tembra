#[path = "support/adapters.rs"]
mod adapters;
mod support;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serde_json::json;
use std::{hint::black_box, time::Duration};
use support::Fixture;
use tembra_core::{
    GetOptions,
    contracts::{PreparedSchema, validate},
};

fn runtime(c: &mut Criterion) {
    adapters::benchmarks(c);
    let mut fixture = Fixture::new(0, false);
    let mut dispatch = c.benchmark_group("dispatch");
    dispatch.throughput(Throughput::Elements(1));
    dispatch.bench_function("native_held", |b| {
        b.iter_batched(
            || fixture.drain(),
            |()| {
                black_box(
                    fixture
                        .driver
                        .run_until(fixture.reference.call::<u64, u64>("native", 1, None))
                        .unwrap(),
                );
            },
            BatchSize::PerIteration,
        )
    });
    dispatch.bench_function("portable_held", |b| {
        b.iter_batched(
            || fixture.drain(),
            |()| {
                black_box(
                    fixture
                        .driver
                        .run_until(
                            fixture
                                .reference
                                .call_json("portable", Some(json!(1)), None),
                        )
                        .unwrap(),
                );
            },
            BatchSize::PerIteration,
        )
    });
    dispatch.bench_function("native_fresh_claim", |b| {
        b.iter_batched(
            || fixture.drain(),
            |()| {
                let reference = fixture
                    .driver
                    .run_until(fixture.host.get(
                        fixture.contract.clone(),
                        json!({}),
                        GetOptions::default(),
                    ))
                    .unwrap();
                black_box(
                    fixture
                        .driver
                        .run_until(reference.call::<u64, u64>("native", 1, None))
                        .unwrap(),
                );
                reference.release();
            },
            BatchSize::PerIteration,
        )
    });
    dispatch.bench_function("portable_fresh_descriptor", |b| {
        b.iter_batched(
            || fixture.drain(),
            |()| {
                let reference = fixture
                    .driver
                    .run_until(fixture.host.get(
                        support::contract(),
                        json!({}),
                        GetOptions::default(),
                    ))
                    .unwrap();
                black_box(
                    fixture
                        .driver
                        .run_until(reference.call_json("portable", Some(json!(1)), None))
                        .unwrap(),
                );
                reference.release();
            },
            BatchSize::PerIteration,
        )
    });
    dispatch.finish();
    eprintln!(
        "{}",
        serde_json::json!({"observation_health":fixture.health(),"group":"dispatch"})
    );

    let schema = json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false});
    let value = json!({"count": 1});
    let prepared = PreparedSchema::new(&schema).unwrap();
    let mut validation = c.benchmark_group("validation");
    validation.bench_function("schema_reused", |b| {
        b.iter(|| black_box(prepared.validate(black_box(&value))).unwrap())
    });
    validation.bench_function("schema_fresh", |b| {
        b.iter(|| black_box(validate(black_box(&schema), black_box(&value))).unwrap())
    });
    validation.finish();

    let mut state = c.benchmark_group("state_fanout");
    for fanout in [0, 1, 16, 64] {
        let mut fixture = Fixture::new(fanout, false);
        state.bench_with_input(BenchmarkId::from_parameter(fanout), &fanout, |b, _| {
            b.iter_batched(
                || fixture.drain(),
                |()| {
                    black_box(
                        fixture
                            .driver
                            .run_until(fixture.reference.call::<u64, u64>("run", 1, None))
                            .unwrap(),
                    );
                },
                BatchSize::PerIteration,
            )
        });
        fixture.report(&format!("state_fanout/{fanout}"));
    }
    state.finish();
    c.bench_function("registry/snapshot_lookup", |b| {
        b.iter_batched(
            || fixture.drain(),
            |()| {
                black_box(
                    fixture
                        .registry
                        .table(None)
                        .unwrap()
                        .get("counter")
                        .unwrap()
                        .data(),
                );
            },
            BatchSize::PerIteration,
        )
    });
    fixture.report("dispatch_and_registry");
    c.bench_function("lifecycle/construct_activate_close", |b| {
        let mut checked = 0_u64;
        let mut last_health = serde_json::Value::Null;
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let start = std::time::Instant::now();
                let mut fixture = Fixture::new(0,false);
                fixture.close_runtime();
                elapsed += start.elapsed();
                fixture.drain();
                last_health = fixture.health();
                checked += 1;
            }
            elapsed
        });
        eprintln!("{}",serde_json::json!({"case":"lifecycle","fixtures_validated_after_cleanup":checked,"last_observation_health":last_health}));
    });
    let mut composed = c.benchmark_group("composed");
    for concurrency in [1, 16, 256] {
        let mut fixture = Fixture::new(16, true);
        composed.throughput(Throughput::Elements(concurrency * 4));
        composed.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                b.iter_batched(
                    || fixture.drain(),
                    |()| {
                        black_box(fixture.turns(concurrency as usize));
                    },
                    BatchSize::PerIteration,
                )
            },
        );
        fixture.report(&format!("composed/{concurrency}"));
    }
    composed.finish();
}
fn configuration() -> Criterion {
    let output = std::env::var_os("TEMBRA_BENCH_OUTPUT")
        .expect("set an external TEMBRA_BENCH_OUTPUT directory or use the benchmark runner");
    let output = std::path::Path::new(&output);
    assert!(
        output.is_absolute() && !output.starts_with(std::env::current_dir().unwrap()),
        "benchmark output must be outside the checkout"
    );
    Criterion::default()
        .output_directory(output)
        .sample_size(30)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
}
criterion_group! { name = benches; config = configuration(); targets = runtime }
criterion_main!(benches);
