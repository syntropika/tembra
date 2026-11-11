//! Standard WASI imports of prepared SDK source Orbs under actual kernel admission and
//! cancellation. The Orbs are `tests/source/test-{standard,standard-peer,quit,builtins-*}`.
#[path = "support/source.rs"]
mod source;
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use source::fixture;
use std::{
    rc::Rc,
    time::{Duration, Instant},
};
use tembra::{Host, Reference};
use tembra_executor_wasm::{Engine, FilesystemGrant, Grants, Install, Limits, SystemCapability};

/// The read-only `/data` mount the standard fixtures use.
fn data_grants() -> Grants {
    Grants {
        system: [SystemCapability::Clocks].into(),
        filesystem: vec![FilesystemGrant {
            provider: "data".into(),
            mount: "/data".into(),
            writable: false,
        }],
        ..Grants::default()
    }
}

/// Call through an admitted kernel root; no test bypasses the native invocation owner.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: Rc<Reference>,
    method: &'static str,
) -> tembra::Result<Value> {
    pool.run_until(host.root("test", move |context| {
        async move {
            reference
                .call::<_, Value>(&context, method, json!([]))
                .await
        }
        .boxed_local()
    }))
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_standard_cancellation_preserves_owned_descriptors_and_retires_the_store() {
    source_standard(false);
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn interpreted_source_standard_cancellation_preserves_owned_descriptors_and_retires_the_store() {
    source_standard(true);
}

/// The same admitted operation and cancellation run under native code and portable Pulley.
fn source_standard(interpreted: bool) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("bytes"), [0, 255, 128, 1]).unwrap();
    let (logs, observed) = async_channel::unbounded();
    let limits = Limits {
        // Debug Pulley initialization is slower; cancellation still has its own two-second gate.
        timeout_ms: if interpreted { 30_000 } else { 3000 },
        ..Limits::default()
    };
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
    .filesystem("data", directory.path())
    .unwrap()
    .on_log(move |record| {
        let _ = logs.try_send(record.message.clone());
    });
    let artifact = fixture("test-standard");
    // Standard imports need their grants: an ungranted installation is refused.
    assert!(engine.load(&artifact, Install::default()).is_err());
    let mut install = Install::default();
    install.grants = data_grants();
    let orb = engine.load(&artifact, install).unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let reference = Rc::new(
        host.get(
            "standard::test:standard/checks",
            vec!["main".into()],
            "test",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    assert_eq!(
        call(&mut pool, &host, reference.clone(), "open").unwrap(),
        "4"
    );
    let held = reference.clone();
    let started = Instant::now();
    let outcome = pool.run_until(host.root("test", move |context| {
        async move {
            let cancellation = context.clone();
            let holding = held.call::<_, Value>(&context, "hold", json!([]));
            // Cancel once the guest logged that it waits. Without the marker the call ends at
            // its wall timeout and the elapsed-time assertion below fails instead of hanging.
            let cancelling = async move {
                while let Ok(message) = observed.recv().await {
                    if message.contains("standard:waiting") {
                        cancellation.cancel().unwrap();
                        break;
                    }
                }
                futures::future::pending::<()>().await
            };
            match futures::future::select(std::pin::pin!(holding), std::pin::pin!(cancelling)).await
            {
                futures::future::Either::Left((outcome, _)) => outcome,
                futures::future::Either::Right(((), _)) => unreachable!(),
            }
        }
        .boxed_local()
    }));
    // The cancelled root fails, or the guest reports the revoked wait through its `result` lane;
    // it never finishes the wait.
    assert!(
        outcome.as_ref().map_or(true, |value| value["tag"] == "err"),
        "{outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancellation waited for the callback wall timeout"
    );
    assert_eq!(
        call(&mut pool, &host, reference.clone(), "size").unwrap(),
        "4"
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// Load one granted prepared source Orb and return its admitted `checks` reference.
fn orb(
    engine: &Engine,
    host: &Host,
    pool: &mut LocalPool,
    artifact: &str,
    service: &str,
) -> Rc<Reference> {
    let mut install = Install::default();
    install.grants = data_grants();
    host.install(vec![engine.load(&fixture(artifact), install).unwrap()])
        .unwrap();
    let reference = Rc::new(host.get(service, vec!["main".into()], "test").unwrap());
    pool.run_until(reference.ready()).unwrap();
    reference
}

/// Process-wide filesystem admission is shared by every Orb: a callback that returns with
/// admitted calls outstanding must give the slots back before its Orb runs again.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn abandoned_standard_calls_release_process_admission_while_their_orb_is_idle() {
    for interpreted in [false, true] {
        abandoned(interpreted);
    }
}

fn abandoned(interpreted: bool) {
    let directory = tempfile::tempdir().unwrap();
    let limits = Limits {
        timeout_ms: if interpreted { 30_000 } else { 5000 },
        ..Limits::default()
    };
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
    .filesystem("data", directory.path())
    .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let first = orb(
        &engine,
        &host,
        &mut pool,
        "test-standard",
        "standard::test:standard/checks",
    );
    let second = orb(
        &engine,
        &host,
        &mut pool,
        "test-standard-peer",
        "standard-peer::test:standard-peer/checks",
    );
    for _ in 0..3 {
        assert_eq!(
            call(&mut pool, &host, first.clone(), "abandon").unwrap(),
            "abandoned"
        );
        // The first Orb stays idle; the second needs most of the process admission now.
        assert_eq!(
            call(&mut pool, &host, second.clone(), "burst").unwrap(),
            "96"
        );
    }
    assert_eq!(
        call(&mut pool, &host, first.clone(), "burst").unwrap(),
        "96"
    );
    first.release().unwrap();
    second.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

/// `wasi:cli/exit` in a source Orb retires only that revision with its exact status.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_exit_fails_only_its_revision_with_the_exact_status() {
    for interpreted in [false, true] {
        let engine = if interpreted {
            Engine::interpreted(Limits {
                timeout_ms: 30_000,
                ..Limits::default()
            })
        } else {
            Engine::new(Limits {
                timeout_ms: 3000,
                ..Limits::default()
            })
        }
        .unwrap();
        let artifact = fixture("test-quit");
        assert!(engine.load(&artifact, Install::default()).is_err());
        let mut install = Install::default();
        install.grants = Grants {
            system: [SystemCapability::Cli].into(),
            ..Grants::default()
        };
        let orb = engine.load(&artifact, install).unwrap();
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![orb]).unwrap();
        let reference = Rc::new(
            host.get("quit::test:quit/run", vec!["main".into()], "test")
                .unwrap(),
        );
        pool.run_until(reference.ready()).unwrap();
        let error = call(&mut pool, &host, reference.clone(), "go")
            .unwrap_err()
            .to_string();
        assert!(error.contains("orb exited with status 4"), "{error}");
        // The exit retired the revision: a later call is refused, never served by guest code.
        let again = call(&mut pool, &host, reference.clone(), "go")
            .unwrap_err()
            .to_string();
        assert!(!again.contains("caught"), "{again}");
        let _ = reference.release();
        // Closing may report the retired revision; its Store is physically gone either way.
        let _ = pool.run_until(host.close());
        assert_eq!(engine.instances(), 0);
        assert_eq!(engine.resources(), 0);
    }
}

/// Install one prepared source Orb and return its admitted `checks` reference.
fn source_orb(
    engine: &Engine,
    host: &Host,
    pool: &mut LocalPool,
    artifact: &[u8],
    service: &str,
    grants: Grants,
) -> Rc<Reference> {
    let mut install = Install::default();
    install.grants = grants;
    host.install(vec![engine.load(artifact, install).unwrap()])
        .unwrap();
    let reference = Rc::new(host.get(service, vec!["main".into()], "test").unwrap());
    pool.run_until(reference.ready()).unwrap();
    reference
}

/// Ambient JavaScript built-ins never observe host time or entropy without a grant; the public
/// standard clock and seed imports follow the system grants and the engine's configured
/// (stateful) sources.
///
/// Declared breaking semantics: source built-ins are no longer refused per capability. Without
/// the Clocks grant the runtime's internal clocks are frozen at the epoch, and `Math.random` is
/// seeded from a fixed internal value that is never security material, whatever is granted. The
/// configured sources are reached only through the public imports and, with the Clocks grant, the
/// `Date` built-ins.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_builtins_follow_grants_and_configured_sources() {
    for interpreted in [false, true] {
        builtins(interpreted);
    }
}

fn builtins(interpreted: bool) {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    struct Wall(u64);
    impl tembra_executor_wasm::HostWallClock for Wall {
        fn resolution(&self) -> Duration {
            Duration::from_millis(1)
        }
        fn now(&self) -> Duration {
            Duration::from_millis(1_700_000_000_000 + self.0)
        }
    }
    let (walls, seeds) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (wall_count, seed_count) = (walls.clone(), seeds.clone());
    let limits = Limits {
        timeout_ms: if interpreted { 30_000 } else { 3000 },
        ..Limits::default()
    };
    let engine = if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
    .wall_clock(move || Wall(wall_count.fetch_add(1, Ordering::SeqCst) + 1))
    .insecure_random_seed(move || {
        let n = seed_count.fetch_add(1, Ordering::SeqCst) + 1;
        (u128::from(0x1234_5678_u64 + n) << 64) | u128::from(0x9abc_def0_u64 * n)
    });
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let probe = |pool: &mut LocalPool, reference: Rc<Reference>, method: &'static str| -> String {
        call(pool, &host, reference, method)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    };
    let random = |result: &Value| -> f64 {
        let random: f64 = result["random"].as_str().unwrap().parse().unwrap();
        assert!((0.0..1.0).contains(&random), "{random}");
        random
    };

    // No grants: the ambient clock is frozen at the epoch and no configured source is touched.
    let denied = source_orb(
        &engine,
        &host,
        &mut pool,
        &fixture("test-builtins-denied"),
        "denied::test:builtins-denied/checks",
        Grants::default(),
    );
    let result: Value = serde_json::from_str(&probe(&mut pool, denied.clone(), "probe")).unwrap();
    for path in ["construct", "now"] {
        assert_eq!(result[path], "0", "{path}: {result}");
    }
    for path in ["call", "constructor"] {
        assert!(
            result[path].as_str().unwrap().contains("1970"),
            "{path}: {result}"
        );
    }
    random(&result);
    assert_eq!(result["pure"], "29/86400000");
    assert_eq!(walls.load(Ordering::SeqCst), 0);
    assert_eq!(seeds.load(Ordering::SeqCst), 0);

    // The public clock and seed imports require their grants.
    let granted_artifact = fixture("test-builtins-granted");
    assert!(engine.load(&granted_artifact, Install::default()).is_err());
    assert_eq!(walls.load(Ordering::SeqCst), 0);
    assert_eq!(seeds.load(Ordering::SeqCst), 0);

    // Clocks and Random granted.
    let grants = Grants {
        system: [SystemCapability::Clocks, SystemCapability::Random].into(),
        ..Grants::default()
    };
    let granted = source_orb(
        &engine,
        &host,
        &mut pool,
        &granted_artifact,
        "granted::test:builtins-granted/checks",
        grants,
    );
    assert_eq!(
        walls.load(Ordering::SeqCst),
        1,
        "one clock instance for the new Store"
    );
    assert_eq!(
        seeds.load(Ordering::SeqCst),
        1,
        "one seed for the new Store"
    );
    let result: Value = serde_json::from_str(&probe(&mut pool, granted.clone(), "probe")).unwrap();
    assert_eq!(
        result["now"], "1700000000001",
        "the configured clock, not ambient time"
    );
    assert_eq!(result["construct"], "1700000000001");
    for path in ["call", "constructor"] {
        assert!(
            result[path].as_str().unwrap().contains("2023"),
            "{path}: {result}"
        );
    }
    assert_eq!(result["pure"], "29/86400000");
    random(&result);
    // The public standard clock reads the same configured instance.
    assert_eq!(
        probe(&mut pool, granted.clone(), "wall"),
        "1700000000,1000000"
    );
    // The standard insecure seed is the same per-Store value (low, high order per WIT).
    let (high, low) = (0x1234_5678_u64 + 1, 0x9abc_def0_u64);
    assert_eq!(
        probe(&mut pool, granted.clone(), "seed"),
        format!("{low},{high}")
    );
    assert_eq!(walls.load(Ordering::SeqCst), 1);
    assert_eq!(seeds.load(Ordering::SeqCst), 1);
    denied.release().unwrap();
    granted.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}
