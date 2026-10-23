use std::process::Command;
use tembra_core::{Host, HostOptions, LoadOptions, ManualDriver};
use tembra_executor_js::{BundledJsArtifact, Executor, ExecutorOptions};

// V8 can abort while unwinding an isolate-order assertion. A child process makes
// this embedding regression observable without destroying unrelated test evidence.
#[test]
fn another_live_engine_is_refused_until_actual_module_ownership_ends() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "isolate_order_child", "--nocapture"])
        .env("TEMBRA_ISOLATE_ORDER_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "engine admission failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn isolate_order_child() {
    if std::env::var_os("TEMBRA_ISOLATE_ORDER_CHILD").is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("first", serde_json::Value::Null),
        driver.clone(),
    );
    let first = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let artifact = BundledJsArtifact::new(
        "first",
        "export default {name:'first',uses:[],setup(){}};",
        vec!["default".into()],
    )
    .unwrap();
    let facet = driver
        .run_until(first.prepare_for_host(&host, artifact, "default".into()))
        .unwrap();
    driver
        .run_until(
            host.install_executable(facet, LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        Executor::new(driver.clone(), ExecutorOptions::default())
            .err()
            .expect("live engine must be refused")
            .reason,
        "engine_thread"
    );
    drop(first);
    assert_eq!(
        Executor::new(driver.clone(), ExecutorOptions::default())
            .err()
            .expect("installed module retains the actual first engine")
            .reason,
        "engine_thread"
    );
    driver.run_until(host.close()).unwrap();
    drop(host);
    driver.run_until_stalled();
    let next = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let artifact = BundledJsArtifact::new(
        "second",
        "export default {name:'second',uses:[],setup(){}};",
        vec!["default".into()],
    )
    .unwrap();
    let prepared = driver
        .run_until(next.prepare(1, artifact, "default".into()))
        .unwrap();
    drop(prepared);
    drop(next);
    driver.run_until_stalled();
}

#[test]
fn manual_driver_with_entered_tokio_runtime_evaluates_and_closes() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("manual", serde_json::Value::Null),
        driver.clone(),
    );
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let artifact = BundledJsArtifact::new(
        "manual",
        "export default {name:'manual',uses:[],setup(){}};",
        vec!["default".into()],
    )
    .unwrap();
    let facet = driver
        .run_until(executor.prepare_for_host(&host, artifact, "default".into()))
        .unwrap();
    driver
        .run_until(
            host.install_executable(facet, LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    driver.run_until(host.close()).unwrap();
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().contexts, 0);
}

#[test]
fn manual_driver_alone_refuses_missing_tokio_runtime_before_engine_creation() {
    let driver = ManualDriver::new();
    assert_eq!(
        Executor::new(driver, ExecutorOptions::default())
            .err()
            .expect("ambient Tokio is required")
            .reason,
        "tokio_runtime"
    );
}
