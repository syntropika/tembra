//! B1: callback association of prepared source Orbs whose QuickJS job queue is shared by every
//! call of one Store. The host serializes calls through full task-group completion; the
//! runtime refuses work a call leaves for a later call.
#[path = "support/source.rs"]
mod source;
use futures::FutureExt;
use serde_json::{Value, json};
use source::{Installed, engines, fixture};
use std::time::Duration;
use tembra_executor_wasm::Install;

const PROBE: &str = "b1::tembra-test:b1/probe@1.0.0";

/// A detached timer continuation of alice's call runs under alice's authority, and bob's call
/// starts only after alice's task group has physically finished.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn detached_work_finishes_under_its_own_call_before_the_next_call() {
    for engine in engines() {
        let mut installed = Installed::new(&engine, vec![(fixture("b1"), Install::default())]);
        let probe = installed.reference(PROBE, "host");
        installed
            .call(&probe, "alice", "detach", json!([50]))
            .unwrap();
        // Bob's call starts only after alice's continuation ran, and it never saw bob.
        let observed = installed
            .call(&probe, "bob", "observed", json!([]))
            .unwrap();
        let observed = observed.as_str().unwrap();
        assert!(observed.starts_with("ran: "), "{observed}");
        assert!(!observed.contains("bob"), "{observed}");
        probe.release().unwrap();
        assert!(installed.host.errors().is_empty());
        installed.close(&engine);
    }
}

/// Cancelling alice's root revokes the host work her detached continuation still waits for: the
/// continuation observes the refusal under her call, and bob never runs it.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn cancelling_a_call_revokes_its_detached_work() {
    for engine in engines() {
        let mut installed = Installed::new(&engine, vec![(fixture("b1"), Install::default())]);
        let probe = installed.reference(PROBE, "host");
        let called = probe.clone();
        let outcome = installed
            .pool
            .run_until(installed.host.root("alice", move |context| {
                async move {
                    let call = called.call_portable::<_, Value>(&context, "detach", json!([4000]));
                    let cancel = async {
                        futures_timer::Delay::new(Duration::from_millis(200)).await;
                        context.cancel()
                    };
                    let (call, cancelled) = futures::join!(call, cancel);
                    cancelled?;
                    Ok(call.is_ok())
                }
                .boxed_local()
            }));
        assert!(outcome.is_ok(), "{outcome:?}");
        let observed = installed
            .call(&probe, "bob", "observed", json!([]))
            .unwrap();
        let observed = observed.as_str().unwrap();
        assert!(observed.starts_with("revoked"), "{observed}");
        probe.release().unwrap();
        installed.close(&engine);
    }
}

/// A continuation alice leaves without any host operation is refused in alice's own call: the
/// revision is quarantined, bob's call is refused, and the continuation never runs.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn a_dormant_continuation_quarantines_its_own_call_and_never_runs_later() {
    for engine in engines() {
        let mut installed = Installed::new(&engine, vec![(fixture("b1"), Install::default())]);
        let probe = installed.reference(PROBE, "host");
        let parked = installed.call(&probe, "alice", "park", json!([]));
        let error = parked.expect_err("a pending promise must refuse the call that left it");
        assert!(error.message.contains("trap"), "{error}");
        let poked = installed.call(&probe, "bob", "poke", json!([]));
        assert!(
            poked.is_err(),
            "bob reached a quarantined revision: {poked:?}"
        );
        assert_ne!(poked.ok(), Some(json!("resumed")));
        let _ = probe.release();
        installed.retire(&engine);
    }
}
