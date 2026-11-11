//! Qualify cleanup, scope identity, concurrent calls and termination against the real engine.
//!
//! The Orbs are the prepared SDK sources `tests/source/test-safety` and `test-safety-causal`.
#[path = "support/source.rs"]
mod source;
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use source::fixture;
use std::{
    rc::Rc,
    time::{Duration, Instant},
};
use tembra::Host;
use tembra_executor_wasm::{Contract, Engine, Install, Limits};

const CHECKS: &str = "safe::example:safety/checks";

fn engine(interpreted: bool, timeout_ms: u64) -> Engine {
    let limits = Limits {
        timeout_ms,
        ..Limits::default()
    };
    if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
}

/// Run real callbacks, then prove each Store is released by native shutdown.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn scope_cleanup_and_termination() {
    for interpreted in [false, true] {
        cleanup_and_termination(interpreted);
    }
}

fn cleanup_and_termination(interpreted: bool) {
    let engine = engine(interpreted, 500);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine
            .load(&fixture("test-safety"), Install::default())
            .unwrap(),
    ])
    .unwrap();
    let one = Rc::new(host.get(CHECKS, vec!["one".into()], "alice").unwrap());
    let two = Rc::new(host.get(CHECKS, vec!["two".into()], "bob").unwrap());
    pool.run_until(one.ready()).unwrap();
    pool.run_until(two.ready()).unwrap();
    let calls = (one.clone(), two.clone());
    pool.run_until(host.root("alice", move |context| {
        async move {
            let (one, two) = calls;
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["save-context"]))
                    .await?,
                1
            );
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["stale-context"]))
                    .await?,
                2
            );
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["deferred"]))
                    .await?,
                1
            );
            assert_eq!(
                two.call::<_, Value>(&context, "run", json!(["cached"]))
                    .await?,
                2
            );
            // Detached work finishes before the next call starts; its context ended with its
            // call, so it cannot update the state.
            let start = Instant::now();
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["floating"]))
                    .await?,
                1
            );
            assert!(start.elapsed() < Duration::from_millis(500));
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["read"]))
                    .await?,
                1
            );
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["many"]))
                    .await?,
                256
            );
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["state-checks"]))
                    .await?,
                1
            );
            let (first, second) = futures::join!(
                one.call::<_, Value>(&context, "run", json!(["wait"])),
                one.call::<_, Value>(&context, "run", json!(["wait"]))
            );
            assert_eq!(first?, 1);
            assert_eq!(second?, 1);
            assert_eq!(
                one.call::<_, Value>(&context, "run", json!(["read"]))
                    .await?,
                1
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    let calls = one.clone();
    let creating_host = host.clone();
    pool.run_until(host.root("alice", move |context| {
        async move {
            let running = calls.call::<_, Value>(&context, "run", json!(["long-wait"]));
            let creating = async move {
                futures_timer::Delay::new(Duration::from_millis(10)).await;
                let reference = creating_host.get(CHECKS, vec!["during-call".into()], "alice")?;
                reference.ready().await?;
                reference.release()?;
                Ok::<(), tembra::Error>(())
            };
            let (called, created) = futures::join!(running, creating);
            assert_eq!(called?, 1);
            created?;
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    one.release().unwrap();
    two.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);

    assert!(host.errors().is_empty(), "{:?}", host.errors());

    // A promise no host operation settles and a busy loop are both bounded: the call fails, the
    // revision is quarantined, and closing physically retires the Store.
    for mode in ["hang", "cpu"] {
        let host = Host::new(pool.spawner());
        host.install(vec![
            engine
                .load(&fixture("test-safety"), Install::default())
                .unwrap(),
        ])
        .unwrap();
        let reference = Rc::new(host.get(CHECKS, vec!["main".into()], "alice").unwrap());
        pool.run_until(reference.ready()).unwrap();
        let called = reference.clone();
        let started = Instant::now();
        pool.run_until(host.root("alice", move |context| {
            async move {
                let error = called
                    .call::<_, Value>(&context, "run", json!([mode]))
                    .await
                    .unwrap_err();
                assert!(
                    error.to_string().contains("trap") || error.to_string().contains("timed out"),
                    "{mode}: {error}"
                );
                let error = called
                    .call::<_, Value>(&context, "run", json!(["read"]))
                    .await
                    .unwrap_err();
                assert_eq!(error.reason, "wasm_executor_quarantined", "{mode}: {error}");
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
        // The `cpu` loop ends at the 500 ms callback limit, never at a test or OS deadline.
        assert!(started.elapsed() < Duration::from_secs(10), "{mode}");
        let _ = reference.release();
        let _ = pool.run_until(host.close());
        assert_eq!(engine.instances(), 0, "{mode}");
        assert_eq!(engine.resources(), 0, "{mode}");
    }
}

/// The bridge provider world of `tests/source/test-safety-causal/contract.wit`, verbatim;
/// installation compares the signature, so a drifted copy is refused.
const BRIDGE: &str = r#"package example:safety-causal;

/// Provided natively; the provider tries to create an object of the calling Orb.
interface bridge {
    bounce: async func() -> result<u32, string>;
}

world provider {
    export bridge;
}
"#;

/// `result<u32, string>` in the portable tagged spelling.
#[derive(serde::Serialize)]
#[serde(tag = "tag", content = "val", rename_all = "lowercase")]
enum Bounced {
    Ok(u32),
    Err(String),
}

/// Creating a new object on a causally occupied revision fails within the admission bound
/// without quarantining that revision.
///
/// The native bridge reports the refused creation through its WIT `result`, a business error the
/// source caller returns as its own. (A dependency call failing without a WIT error lane would
/// trap the calling source Orb instead: declared breaking semantics.)
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn causal_factory_wait_is_bounded_and_recoverable() {
    for interpreted in [false, true] {
        causal_factory_wait(interpreted);
    }
}

fn causal_factory_wait(interpreted: bool) {
    let engine = engine(interpreted, 1000);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let creating = host.clone();
    let bridge = tembra::Orb::new("bridge", 1).service(
        tembra::Service::new("bridge::example:safety-causal/bridge", |_| Ok(()))
            .signature(
                Contract::from_source("bridge.wit", BRIDGE, Some("provider"))
                    .unwrap()
                    .plan("bridge", 1, &Default::default())
                    .unwrap()
                    .orb
                    .services
                    .remove(0)
                    .signature,
            )
            .method("bounce", move |_, _, _: Vec<Value>| {
                let host = creating.clone();
                async move {
                    let created = async {
                        let next = host.get(
                            "safe::example:safety-causal/checks",
                            vec!["reentrant".into()],
                            "alice",
                        )?;
                        next.ready().await?;
                        next.release()?;
                        Ok::<_, tembra::Error>(0_u32)
                    }
                    .await;
                    Ok(match created {
                        Ok(value) => Bounced::Ok(value),
                        Err(error) => Bounced::Err(error.to_string()),
                    })
                }
            }),
    );
    host.install(vec![
        bridge,
        engine
            .load(
                &fixture("test-safety-causal"),
                Install::default().requires("example:safety-causal/bridge", "bridge", 1),
            )
            .unwrap(),
    ])
    .unwrap();
    let reference = Rc::new(
        host.get(
            "safe::example:safety-causal/checks",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    let called = reference.clone();
    let started = Instant::now();
    pool.run_until(host.root("alice", move |context| {
        async move {
            let refused = called
                .call::<_, Value>(&context, "reenter", json!([]))
                .await?;
            assert_eq!(refused["tag"], "err", "{refused}");
            assert!(
                refused["val"]
                    .as_str()
                    .unwrap()
                    .contains("factory admission"),
                "{refused}"
            );
            assert_eq!(
                called.call::<_, Value>(&context, "read", json!([])).await?,
                1
            );
            Ok(())
        }
        .boxed_local()
    }))
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
