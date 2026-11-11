//! Replace prepared source revisions while the kernel retains compatible state, and refuse
//! candidates whose validator or resource opener fails without disturbing the live revision.
#[path = "support/source.rs"]
mod source;
use serde_json::json;
use source::{Installed, engines, fixture};
use tembra_executor_wasm::Install;

const LEDGER: &str = "ledger::tembra-test:reload/ledger@1.0.0";

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn reload_retains_state_and_refuses_failing_candidates() {
    for engine in engines() {
        let mut installed =
            Installed::new(&engine, vec![(fixture("reload-v1"), Install::default())]);
        let ledger = installed.reference(LEDGER, "alice");
        assert_eq!(
            installed.call(&ledger, "alice", "add", json!([5])).unwrap(),
            5
        );
        assert_eq!(
            installed
                .call(&ledger, "alice", "revision", json!([]))
                .unwrap(),
            1
        );

        // A compatible revision takes over with the retained total.
        let replacement = engine
            .load(&fixture("reload-v2"), Install::default())
            .unwrap();
        installed
            .pool
            .run_until(installed.host.reload(vec![replacement], 5000))
            .unwrap();
        assert_eq!(
            installed
                .call(&ledger, "alice", "revision", json!([]))
                .unwrap(),
            2
        );
        assert_eq!(
            installed.call(&ledger, "alice", "add", json!([1])).unwrap(),
            6
        );

        // The candidate's validator refuses the retained total: the live revision keeps serving.
        let validating = engine
            .load(&fixture("reload-validating"), Install::default())
            .unwrap();
        assert!(
            installed
                .pool
                .run_until(installed.host.reload(vec![validating], 5000))
                .is_err()
        );
        assert_eq!(
            installed
                .call(&ledger, "alice", "revision", json!([]))
                .unwrap(),
            2
        );

        // The candidate's resource opener fails: the reload aborts and its Store is retired.
        let failing = engine
            .load(&fixture("reload-failing"), Install::default())
            .unwrap();
        assert!(
            installed
                .pool
                .run_until(installed.host.reload(vec![failing], 5000))
                .is_err()
        );
        assert_eq!(
            installed.call(&ledger, "alice", "add", json!([1])).unwrap(),
            7
        );
        assert_eq!(
            installed
                .call(&ledger, "alice", "revision", json!([]))
                .unwrap(),
            2
        );
        assert_eq!(engine.instances(), 1);

        ledger.release().unwrap();
        assert!(
            installed.host.errors().is_empty(),
            "{:?}",
            installed.host.errors()
        );
        installed.close(&engine);
    }
}

/// Diagnostic split: a reload with no live object of the replaced revision.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn reload_without_live_objects_replaces_the_revision() {
    for engine in engines() {
        let mut installed =
            Installed::new(&engine, vec![(fixture("reload-v1"), Install::default())]);
        let replacement = engine
            .load(&fixture("reload-v2"), Install::default())
            .unwrap();
        installed
            .pool
            .run_until(installed.host.reload(vec![replacement], 5000))
            .unwrap();
        let ledger = installed.reference(LEDGER, "alice");
        assert_eq!(
            installed
                .call(&ledger, "alice", "revision", json!([]))
                .unwrap(),
            2
        );
        ledger.release().unwrap();
        installed.close(&engine);
    }
}

/// The same replacement of a compiled Rust component, through the common component worker.
#[test]
#[ignore = "requires TEMBRA_WASM_PROVIDER"]
fn reload_replaces_a_compiled_revision() {
    let component =
        std::fs::read(std::env::var_os("TEMBRA_WASM_PROVIDER").expect("TEMBRA_WASM_PROVIDER"))
            .unwrap();
    let contract = tembra_executor_wasm::Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/wasm-orb"),
        Some("arithmetic-provider"),
    )
    .unwrap();
    let artifact = contract
        .component_artifact(&component, "math", 1, &Default::default())
        .unwrap();
    for engine in engines() {
        let mut installed = Installed::new(&engine, vec![(artifact.clone(), Install::default())]);
        let replacement = engine.load(&artifact, Install::default()).unwrap();
        installed
            .pool
            .run_until(installed.host.reload(vec![replacement], 5000))
            .unwrap();
        let math = installed.reference("math::example:integrated/arithmetic@1.0.0", "alice");
        assert_eq!(
            installed
                .call(&math, "alice", "double", json!([21]))
                .unwrap(),
            42
        );
        math.release().unwrap();
        installed.close(&engine);
    }
}
