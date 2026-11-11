//! Qualify native cancellation of prepared SDK source callbacks against the real engine. The Orb
//! is `tests/source/test-cancellation`.
#[path = "support/source.rs"]
mod source;
use async_channel::Receiver;
use futures::{
    FutureExt,
    channel::oneshot,
    executor::LocalPool,
    future::{Either, select},
    task::LocalSpawnExt,
};
use serde_json::{Value, json};
use std::{rc::Rc, time::Duration};
use tembra::Host;
use tembra_executor_wasm::{Engine, Install, Limits};

/// A bound that only a regressed implementation reaches; passing runs never wait for it.
const STALL: Duration = Duration::from_secs(3);

/// Native code or portable Pulley, forwarding attributed guest logs to the test thread.
fn engine(interpreted: bool) -> (Engine, Receiver<String>) {
    let (send, receive) = async_channel::unbounded();
    let engine = if interpreted {
        Engine::interpreted(Limits::default())
    } else {
        Engine::new(Limits::default())
    }
    .unwrap()
    .on_log(move |record| {
        let _ = send.try_send(record.message.clone());
    });
    (engine, receive)
}

/// Drive the pool until a marker arrives, returning every earlier marker for ordering checks.
fn until_marker(pool: &mut LocalPool, logs: &Receiver<String>, marker: &str) -> Vec<String> {
    let mut seen = Vec::new();
    loop {
        let next = pool.run_until(async {
            let message = std::pin::pin!(logs.recv());
            let deadline = std::pin::pin!(futures_timer::Delay::new(STALL));
            match select(message, deadline).await {
                Either::Left((message, _)) => message.ok(),
                Either::Right(_) => None,
            }
        });
        let message = next.unwrap_or_else(|| panic!("marker {marker} not observed; saw {seen:?}"));
        if message == marker {
            return seen;
        }
        seen.push(message);
    }
}

/// Collect markers that are already buffered without waiting for more guest work.
fn drained(logs: &Receiver<String>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Ok(message) = logs.try_recv() {
        seen.push(message);
    }
    seen
}

/// Spawn one root call and return its issued context and eventual result.
fn spawn_call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<tembra::Reference>,
    principal: &'static str,
    method: &'static str,
) -> (tembra::Context, oneshot::Receiver<tembra::Result<Value>>) {
    let (issued, context) = oneshot::channel();
    let (finished, finish) = oneshot::channel();
    let host = host.clone();
    let reference = reference.clone();
    pool.spawner()
        .spawn_local(async move {
            let result = host
                .root(principal, move |context| {
                    async move {
                        let _ = issued.send(context.clone());
                        reference
                            .call::<_, Value>(&context, method, json!([]))
                            .await
                    }
                    .boxed_local()
                })
                .await;
            let _ = finished.send(result);
        })
        .unwrap();
    (pool.run_until(context).unwrap(), finish)
}

/// A cancelled call queued behind a live callback rejects before its guest code runs.
#[test]
#[ignore = "requires TEMBRA_WASM_FIXTURES"]
fn cancelled_queued_source_call_rejects_before_admission() {
    for interpreted in [false, true] {
        queued_cancellation(interpreted);
    }
}

fn queued_cancellation(interpreted: bool) {
    let (engine, logs) = engine(interpreted);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        engine
            .load(&source::fixture("test-cancellation"), Install::default())
            .unwrap(),
    ])
    .unwrap();
    let reference = Rc::new(
        host.get(
            "probe::example:cancellation/run",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();

    // The first root occupies the revision until its own cancellation settles the wait.
    let (context_a, finish_a) = spawn_call(&mut pool, &host, &reference, "alice", "slow");
    until_marker(&mut pool, &logs, "slow:started");

    // The second root passes kernel admission and parks on the revision's admission queue.
    let (context_b, finish_b) = spawn_call(&mut pool, &host, &reference, "bob", "read");
    pool.run_until_stalled();
    context_b.cancel().unwrap();

    let settled = pool.run_until(async {
        let deadline = std::pin::pin!(futures_timer::Delay::new(STALL));
        match select(finish_b, deadline).await {
            Either::Left((result, _)) => Some(result.unwrap()),
            Either::Right(_) => None,
        }
    });
    let error = settled
        .expect("a cancelled queued call stayed queued behind the running callback")
        .expect_err("a cancelled queued call executed guest code");
    assert_eq!(error.code, tembra::Code::Cancelled, "{error}");
    let early = drained(&logs);
    assert!(
        !early.iter().any(|message| message == "read:executed"),
        "guest code ran for a cancelled queued call: {early:?}"
    );

    // Admitted work keeps physical ownership; it settles only through its own root.
    assert!(
        !early.iter().any(|message| message == "slow:cancelled"),
        "queued cancellation reached the admitted callback: {early:?}"
    );
    context_a.cancel().unwrap();
    until_marker(&mut pool, &logs, "slow:cancelled");
    let _ = pool.run_until(finish_a).unwrap();

    // The same Store remains live for a later root.
    let live = reference.clone();
    let value = pool
        .run_until(host.root("carol", move |context| {
            async move { live.call::<_, Value>(&context, "read", json!([])).await }.boxed_local()
        }))
        .unwrap();
    assert_eq!(value, 7);
    let late = drained(&logs);
    assert_eq!(
        late.iter()
            .filter(|message| *message == "read:executed")
            .count(),
        1,
        "only the live call executed guest code: {late:?}"
    );

    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(engine.instances(), 0);
}
