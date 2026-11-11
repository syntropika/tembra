//! `Context.get` from a real source Orb on the packaged interpreter.
//!
//! The router and keyed counter Orbs are the packaged TypeScript fixtures
//! (`<TEMBRA_WASM_FIXTURES>/keyed-router.wasm` and `keyed.wasm`, from
//! `packages/ts/sdk/test/fixtures/wasm-keyed`). The router selects counter instances at runtime
//! under the caller's root and principal; every refusal is observed through the guest's own API.
//! Every case runs on Cranelift and Pulley. `test/context-acquire-source.test.ts` runs the same
//! fixtures on the SDK host.
use futures::{
    FutureExt,
    channel::oneshot,
    executor::LocalPool,
    future::{Either, select},
};
use serde_json::{Value, json};
use std::{
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};
use tembra::Host;
use tembra_executor_wasm::{Engine, Install, Limits};

const COUNTER: &str = "example:keyed/counter@1.0.0";
const ROUTER: &str = "router::example:keyed/router@1.0.0";

/// The packaged fixture artifact `<TEMBRA_WASM_FIXTURES>/<name>.wasm`.
fn fixture(name: &str) -> Vec<u8> {
    let directory = std::env::var_os("TEMBRA_WASM_FIXTURES").expect("TEMBRA_WASM_FIXTURES");
    std::fs::read(std::path::Path::new(&directory).join(format!("{name}.wasm"))).unwrap()
}

/// Guest console records, and a signal for the router's resource close outcome.
#[derive(Default)]
struct Logs {
    messages: Mutex<Vec<String>>,
    closed: Mutex<Option<oneshot::Sender<String>>>,
}

fn engine(interpreted: bool, logs: Arc<Logs>) -> Engine {
    let limits = Limits {
        timeout_ms: 30_000,
        ..Limits::default()
    };
    if interpreted {
        Engine::interpreted(limits)
    } else {
        Engine::new(limits)
    }
    .unwrap()
    .on_log(move |record| {
        logs.messages.lock().unwrap().push(record.message.clone());
        if record.message.starts_with("close-acquire ")
            && let Some(closed) = logs.closed.lock().unwrap().take()
        {
            let _ = closed.send(record.message.clone());
        }
    })
}

fn scenario(interpreted: bool) {
    let logs = Arc::new(Logs::default());
    let (closed, close_outcome) = oneshot::channel();
    *logs.closed.lock().unwrap() = Some(closed);
    let engine = engine(interpreted, logs.clone());
    let counters = engine.load(&fixture("keyed"), Install::default()).unwrap();
    let router = engine
        .load(
            &fixture("keyed-router"),
            Install::default().requires(COUNTER, "keyed", 1),
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![counters, router]).unwrap();
    let reference = Rc::new(host.get(ROUTER, vec!["main".into()], "host").unwrap());
    pool.run_until(reference.ready()).unwrap();
    let live = |host: &Host| host.kernel().diagnostics().references;
    // The adapter's portable boundary decodes typed Rust results; `call` would require the
    // provider's own native method types.
    let mut call = |principal: &str, method: &'static str, args: Value| -> String {
        let reference = reference.clone();
        pool.run_until(host.root(principal, move |context| {
            async move { reference.call_portable(&context, method, args).await }.boxed_local()
        }))
        .unwrap()
    };

    // Two handles of one key share its instance; another key and another principal do not.
    assert_eq!(
        call("alice", "route", json!(["a", "b", 2])),
        "2,4,2 a@alice b@alice"
    );
    assert_eq!(
        call("bob", "route", json!(["a", "b", 2])),
        "2,4,2 a@bob b@bob"
    );
    // Each root released its claims: only the application's router reference remains.
    assert_eq!(live(&host), 1);

    let probe = |call: &mut dyn FnMut(&str, &'static str, Value) -> String, name: &str| {
        call("alice", "probe", json!([name])).to_lowercase()
    };
    for (name, expected) in [
        // A context retained past its callback is refused by the guest itself.
        ("retained-context", "refused: context callback has ended"),
        // A reference retained past its root is refused by the host in a later root.
        ("retained-reference", "belongs to another root"),
        // Only selected WIT Orb imports can be acquired, and keys are lists of strings.
        ("unknown-import", "not a selected wit orb import"),
        // The SDK refuses a malformed key before the host is asked.
        (
            "key-kind",
            "refused: a reference key is a string or a list of strings",
        ),
        // Explicit release is idempotent and observable while the root lasts.
        ("release", "released"),
    ] {
        let outcome = probe(&mut call, name);
        assert!(outcome.contains(expected), "{name}: {outcome}");
    }
    drop(call);
    assert_eq!(live(&host), 1);

    // Cancelling the root while the `slow` instance is unready releases the claim at once.
    let observer = host.clone();
    let slow = reference.clone();
    let (outcome, during) = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let probe = slow.call_portable::<_, String>(&context, "probe", json!(["slow"]));
                let cancel = async {
                    futures_timer::Delay::new(Duration::from_millis(500)).await;
                    context.cancel()
                };
                let (outcome, cancelled) = futures::join!(probe, cancel);
                cancelled?;
                Ok((outcome, observer.kernel().diagnostics().references))
            }
            .boxed_local()
        }))
        .unwrap();
    assert!(
        !matches!(&outcome, Ok(value) if value == "accepted"),
        "{outcome:?}"
    );
    assert_eq!(during, 1, "the cancelled acquisition left a live claim");
    assert_eq!(live(&host), 1);

    // Releasing the router disposes its object while the host stays open. Its resource close has
    // a cleanup context without an admitted invocation, so its `Context.get` is refused there,
    // not because the host is shutting down.
    reference.release().unwrap();
    let timeout = futures_timer::Delay::new(Duration::from_secs(20));
    let outcome = match pool.run_until(select(close_outcome, timeout)) {
        Either::Left((outcome, _)) => outcome.expect("close outcome sender dropped"),
        Either::Right(_) => panic!("the router's resource close did not run"),
    };
    assert!(
        outcome.contains("refused") && outcome.contains("admitted invocation"),
        "{outcome}"
    );
    assert!(
        !logs
            .messages
            .lock()
            .unwrap()
            .iter()
            .any(|message| message.starts_with("activate closing@")),
        "a close callback activated a provider instance"
    );
    assert_eq!(live(&host), 0);

    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn source_context_get_selects_root_scoped_instances() {
    scenario(false);
}

#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn pulley_source_context_get_selects_root_scoped_instances() {
    scenario(true);
}
