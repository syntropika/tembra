use criterion::{BatchSize, Criterion, Throughput};
use serde_json::json;
use std::{collections::BTreeMap, rc::Rc, time::Duration};
#[cfg(feature = "observability-bench")]
use tembra_core::ObservationOptions;
use tembra_core::{
    GetOptions, Host, HostOptions, LoadOptions,
    contracts::{Contract, Member, ValueSchema},
};
use tembra_executor_js::{BundledJsArtifact, Executor, ExecutorOptions};
use tembra_native::TokioDriver;

fn health(host: &Host) {
    #[cfg(not(feature = "observability-bench"))]
    let _ = host;
    #[cfg(feature = "observability-bench")]
    if let Some(observations) = host.observations() {
        let _ = observations.drain(1_048_576);
        let health = observations.statistics();
        assert_eq!(health.dropped_events, 0, "recording sample lost events");
        assert_eq!(
            health.identifier_exhaustion, 0,
            "recording sample lost identity"
        );
    }
}

fn main() {
    let mut criterion = Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    if let Ok(directory) = std::env::var("TEMBRA_BENCH_OUTPUT") {
        let directory = std::path::PathBuf::from(directory);
        assert!(directory.is_absolute(), "benchmark output must be absolute");
        assert!(!directory.starts_with(std::env::current_dir().unwrap()));
        criterion = criterion.output_directory(&directory);
    }
    let mut criterion = criterion.configure_from_args();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let driver = TokioDriver::new();
    let options = HostOptions::new("executor-benchmark", json!(null));
    #[cfg(feature = "observability-bench")]
    let options = if let Ok(interval) = std::env::var("TEMBRA_OBSERVABILITY_SAMPLE_EVERY") {
        options.with_observability(
            ObservationOptions::new(
                std::env::var("TEMBRA_OBSERVABILITY_CAPACITY")
                    .map_or(131_072, |value| value.parse().unwrap()),
                interval.parse().unwrap(),
            )
            .unwrap(),
        )
    } else {
        options
    };
    let host = Host::new(options, driver.clone());
    let executor = runtime
        .block_on(
            driver.run_until(async { Executor::new(driver.clone(), ExecutorOptions::default()) }),
        )
        .unwrap();
    let contract = Rc::new(
        Contract::local(
            "benchmark.executor.dispatch",
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                    result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                },
            )]),
        )
        .unwrap(),
    );
    let source = format!(
        "const token={};export default {{name:'executor-benchmark',uses:[],setup(_s,d){{d.serve(token,()=>({{state:{{}},methods:{{async run(_ctx,value){{await Promise.resolve();return value+1}}}}}}))}}}};",
        serde_json::to_string(contract.descriptor()).unwrap()
    );
    let artifact =
        BundledJsArtifact::new("executor-benchmark", source, vec!["default".into()]).unwrap();
    let reference = runtime.block_on(driver.run_until(async {
        #[cfg(feature = "observability-bench")]
        let prepare = executor.prepare_for_host(&host, artifact, "default".into());
        #[cfg(not(feature = "observability-bench"))]
        let prepare = executor.prepare(host.id(), artifact, "default".into());
        let facet = prepare.await.unwrap();
        let startup = host
            .install_executable(facet, LoadOptions::default())
            .unwrap();
        let reference = host
            .get(contract.clone(), json!({}), GetOptions::default())
            .await
            .unwrap();
        startup.await.unwrap();
        reference
    }));
    let mut group = criterion.benchmark_group("embedded_v8");
    group.throughput(Throughput::Elements(1));
    group.bench_function("portable_held_async_method", |bench| {
        bench.iter_batched(
            || health(&host),
            |()| {
                assert_eq!(
                    runtime
                        .block_on(driver.run_until(reference.call_json(
                            "run",
                            Some(json!(1)),
                            None
                        )))
                        .unwrap(),
                    Some(json!(2))
                );
            },
            BatchSize::PerIteration,
        );
    });
    health(&host);
    group.bench_function("portable_fresh_reference", |bench| {
        bench.iter_batched(
            || health(&host),
            |()| {
                runtime.block_on(driver.run_until(async {
                    let fresh = host
                        .get(contract.clone(), json!({}), GetOptions::default())
                        .await
                        .unwrap();
                    assert_eq!(
                        fresh.call_json("run", Some(json!(1)), None).await.unwrap(),
                        Some(json!(2))
                    );
                    fresh.release();
                }));
            },
            BatchSize::PerIteration,
        );
    });
    health(&host);
    group.finish();
    health(&host);
    reference.release();
    runtime.block_on(driver.run_until(host.close())).unwrap();
    health(&host);
    #[cfg(feature = "observability-bench")]
    let statistics = serde_json::to_value(
        host.observations()
            .map(|observations| observations.statistics()),
    )
    .unwrap();
    #[cfg(not(feature = "observability-bench"))]
    let statistics = serde_json::Value::Null;
    let stats = executor.stats();
    assert_eq!(stats.physical_jobs, 0);
    assert_eq!(stats.contexts, 0);
    eprintln!(
        "{}",
        json!({"case":"embedded_v8_cleanup", "observations":statistics,
        "executor":{"physical_jobs":stats.physical_jobs,"contexts":stats.contexts,"callbacks":stats.callbacks,"evaluations":stats.evaluations}})
    );
    criterion.final_summary();
}
