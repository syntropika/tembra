//! `metering::orb(..)` in a native host: the Orb imports the meter interface of another Orb,
//! plainly and by key, and natively each imported function calls the provider the host names in
//! `metering::Providers` through the kernel, as a component's import does. Here the provider is
//! `meters`, a native Orb of the same crate; `crates/executor-wasm/tests/orb_macro_native.rs` has a
//! compiled one.
use futures::{FutureExt, channel::oneshot, executor::LocalPool};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use tembra::{Code, Context, Host, Orb, Provider, Reference, Service, core::Requirement};
use tembra_orb_native_fixture::{
    metering::{self, gauge},
    meters,
};

const READINGS: &str = "metering::example:metering/readings@1.0.0";
const GAUGE: &str = "meters::example:meter/gauge@1.0.0";

/// Both imports answered by the Orb `meters` at API version 1.
fn providers() -> metering::Providers {
    metering::Providers {
        gauge: Provider::new("meters", 1),
        gauge_ref: Provider::new("meters", 1),
    }
}

/// A host with `orbs` installed, its pool and a reference to the readings' `main` object of
/// `alice`, once ready.
fn host_with(orbs: Vec<Orb>) -> (LocalPool, Host, Rc<Reference>) {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(orbs).unwrap();
    let readings = Rc::new(host.get(READINGS, vec!["main".into()], "alice").unwrap());
    pool.run_until(readings.ready()).unwrap();
    (pool, host, readings)
}

/// Call `method` of `reference` portably, with `arguments`, under a root of `principal`.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    principal: &str,
    method: &'static str,
    arguments: Value,
) -> tembra::Result<Value> {
    let reference = reference.clone();
    pool.run_until(host.root(principal, move |context| {
        async move {
            reference
                .call_portable::<_, Value>(&context, method, arguments)
                .await
        }
        .boxed_local()
    }))
}

/// The Orb requires its provider as the kernel requires the provider of a compiled Orb's imports:
/// the service of the interface with the signature `meters` declares for it, and the interface's
/// functions, once for both imports, which name one Orb. Two imports from one Orb name one API
/// version of it.
#[test]
fn the_orb_requires_the_provider_of_its_imports() {
    let orb = metering::orb(providers()).unwrap();
    let provider = meters::orb_named("meters", 1).unwrap();
    let signature = provider.declaration().services[0]
        .signature
        .clone()
        .unwrap();
    assert_eq!(
        orb.declaration().requires,
        [Requirement {
            signatures: [(GAUGE.to_owned(), signature)].into(),
            hook_signatures: Default::default(),
            orb: "meters".into(),
            api_version: 1,
            methods: [(GAUGE.to_owned(), ["add".into(), "total".into()].into())].into(),
            hooks: Default::default(),
        }]
    );
    let refused = metering::orb(metering::Providers {
        gauge: Provider::new("meters", 1),
        gauge_ref: Provider::new("meters", 2),
    })
    .err()
    .unwrap();
    assert_eq!(refused.code, Code::Invalid, "{refused}");
    assert!(
        refused
            .to_string()
            .contains("inconsistent API version for provider meters"),
        "{refused}"
    );
    // The host refuses the Orb without its provider.
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let missing = host.install(vec![metering::orb(providers()).unwrap()]);
    assert!(
        missing.is_err(),
        "the Orb was installed without its provider"
    );
    pool.run_until(host.close()).unwrap();
}

/// A native consumer calls a native provider through its imports: the plain import reaches the
/// provider's `main` object, of the principal `host`, which the consumer's root holds, as a
/// component's root reference does, so it keeps its total from call to call. The import by key
/// reaches the object of each key, under the principal of the call, whose root holds the claim
/// until the view is dropped, as a component's view does: the root keeps no claim after the
/// method, and the provider's object, whose last claim it was, closes, so the next `get` of its
/// key finds a new one, in the same root too. The arguments and results cross as the boundary's
/// JSON, a record of the provider's interface included, for a portable caller of the consumer and
/// for a local one, through its client.
#[test]
fn a_native_consumer_calls_a_native_provider_through_its_imports() {
    let (mut pool, host, readings) = host_with(vec![
        meters::orb_named("meters", 1).unwrap(),
        metering::orb_named("metering", 1, providers()).unwrap(),
    ]);
    for (method, arguments, result) in [
        ("feed", json!([[2, 3]]), json!(5)),
        ("feed", json!([[4]]), json!(9)),
        (
            "feed-at",
            json!(["a", { "amount": 2, "times": 3 }]),
            json!({ "tag": "ok", "val": 6 }),
        ),
        (
            "feed-at",
            json!(["a", { "amount": 1, "times": 1 }]),
            json!({ "tag": "ok", "val": 1 }),
        ),
    ] {
        assert_eq!(
            call(&mut pool, &host, &readings, "alice", method, arguments)
                .unwrap_or_else(|error| panic!("{method}: {error}")),
            result,
            "{method}"
        );
    }
    let client = metering::ReadingsClient::new(readings.clone());
    let reading = |amount, times| gauge::Reading { amount, times };
    let kernel = host.kernel();
    let (local, claims) = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let fed = client.feed(&context, vec![1]).await?;
                let mut at = Vec::new();
                let mut claims = vec![kernel.diagnostics().references];
                for (key, amount) in [("a", 3), ("a", 4), ("b", 5)] {
                    at.push(
                        client
                            .feed_at(&context, key.into(), reading(amount, 2))
                            .await?,
                    );
                    claims.push(kernel.diagnostics().references);
                }
                Ok(((fed, at), claims))
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(local, (10, vec![Ok(6), Ok(8), Ok(10)]));
    assert_eq!(claims, [claims[0]; 4], "the root kept a claim of a view");
    // The `main` object of `host` holds the total of the plain import.
    let meter = Rc::new(host.get(GAUGE, vec!["main".into()], "host").unwrap());
    pool.run_until(meter.ready()).unwrap();
    assert_eq!(
        call(&mut pool, &host, &meter, "host", "total", json!([])).unwrap(),
        json!(10)
    );
    meter.release().unwrap();
    readings.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// A call of an import that fails ends the method that made it with the failure, as a trap ends a
/// component's call, and the consumer answers its next calls: here the provider, written by hand
/// with the signature `meters` declares, refuses every reading.
#[test]
fn a_failed_import_call_ends_the_method_with_its_failure() {
    let signature = meters::orb_named("meters", 1)
        .unwrap()
        .declaration()
        .services[0]
        .signature
        .clone();
    let broken = Orb::new("meters", 1).service(
        Service::new(GAUGE, |_| Ok(()))
            .signature(signature)
            .method("add", |_, _, (_reading,): (Value,)| async {
                Err::<u64, _>(tembra::Error::invalid("reading", "the meter is broken"))
            })
            .method("total", |_, _, _: [Value; 0]| async { Ok(7_u64) }),
    );
    let (mut pool, host, readings) = host_with(vec![
        broken,
        metering::orb_named("metering", 1, providers()).unwrap(),
    ]);
    let failed = call(&mut pool, &host, &readings, "alice", "feed", json!([[1]])).unwrap_err();
    assert_eq!(
        (failed.code, failed.reason.as_str()),
        (Code::Invalid, "reading"),
        "{failed}"
    );
    assert_eq!(
        call(&mut pool, &host, &readings, "alice", "feed", json!([[]])).unwrap(),
        json!(7)
    );
    readings.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// The signature `meters` declares for its gauge, which a provider written by hand declares too.
fn gauge_signature() -> Option<String> {
    meters::orb_named("meters", 1)
        .unwrap()
        .declaration()
        .services[0]
        .signature
        .clone()
}

/// A `get` whose object fails to activate releases its claim and answers why, as a component's
/// view does: here the provider, written by hand with the signature `meters` declares, fails to
/// open the meter of the key `closed` once the object is acquired, and the root of the call keeps
/// no claim of it.
#[test]
fn a_failed_get_releases_its_claim() {
    let picky = Orb::new("meters", 1).service(
        Service::new(GAUGE, |declaration| {
            let closed = declaration.address()?.key == ["closed"];
            declaration.resource(
                "meter",
                1,
                move || {
                    if closed {
                        return Err(tembra::Error::invalid("key", "no meter is open there"));
                    }
                    Ok(())
                },
                |_, _| async { Ok(()) }.boxed_local(),
            )?;
            Ok(())
        })
        .signature(gauge_signature())
        .method("add", |_, _, (_reading,): (Value,)| async { Ok(1_u64) })
        .method("total", |_, _, _: [Value; 0]| async { Ok(1_u64) }),
    );
    let (mut pool, host, readings) = host_with(vec![
        picky,
        metering::orb_named("metering", 1, providers()).unwrap(),
    ]);
    let client = metering::ReadingsClient::new(readings.clone());
    let kernel = host.kernel();
    let (fed, claims) = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let before = kernel.diagnostics().references;
                let reading = gauge::Reading {
                    amount: 1,
                    times: 1,
                };
                let fed = client.feed_at(&context, "closed".into(), reading).await?;
                Ok((fed, [before, kernel.diagnostics().references]))
            }
            .boxed_local()
        }))
        .unwrap();
    let refused = fed.unwrap_err();
    assert!(refused.contains("no meter is open there"), "{refused}");
    assert_eq!(
        claims[1], claims[0],
        "the root kept the claim of a failed get"
    );
    readings.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// A call of an import that the method stops awaiting is cancelled, as a component's executor
/// cancels an import its callback abandons: the provider's call observes it, and the method ends
/// only once that call has ended. Here the provider, written by hand with the signature `meters`
/// declares, answers `total` once its call is cancelled and a gate the test holds opens.
#[test]
fn an_abandoned_import_call_is_cancelled_and_joined() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let (open, gate) = oneshot::channel::<()>();
    let gate = RefCell::new(Some(gate));
    let seen = events.clone();
    let slow = Orb::new("meters", 1).service(
        Service::new(GAUGE, |_| Ok(()))
            .signature(gauge_signature())
            .method("add", |_, _, (_reading,): (Value,)| async { Ok(0_u64) })
            .method("total", move |_, context: Context, _: [Value; 0]| {
                let seen = seen.clone();
                let gate = gate.borrow_mut().take();
                async move {
                    let signal = context.signal();
                    signal.cancelled().await;
                    let reason = signal.check().unwrap_err().reason;
                    seen.borrow_mut().push(format!("cancelled: {reason}"));
                    if let Some(gate) = gate {
                        let _ = gate.await;
                    }
                    seen.borrow_mut().push("ended".to_owned());
                    Ok(7_u64)
                }
            }),
    );
    let (mut pool, host, readings) = host_with(vec![
        slow,
        metering::orb_named("metering", 1, providers()).unwrap(),
    ]);
    // The plain import's object is ready, so the consumer's call of it is admitted at once.
    let meter = Rc::new(host.get(GAUGE, vec!["main".into()], "host").unwrap());
    pool.run_until(meter.ready()).unwrap();
    let called = readings.clone();
    let answered = events.clone();
    let glanced = host
        .spawn_root("alice", move |context| {
            async move {
                let glanced = called
                    .call_portable::<_, Value>(&context, "glance", json!([]))
                    .await;
                answered.borrow_mut().push("answered".to_owned());
                glanced
            }
            .boxed_local()
        })
        .unwrap();
    // The provider's call sees its own cancellation, while the root lasts, and the method waits.
    pool.run_until_stalled();
    assert_eq!(*events.borrow(), ["cancelled: call"]);
    open.send(()).unwrap();
    let glanced = pool.run_until(glanced).unwrap().unwrap();
    assert_eq!(glanced, json!(0));
    assert_eq!(*events.borrow(), ["cancelled: call", "ended", "answered"]);
    meter.release().unwrap();
    readings.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
