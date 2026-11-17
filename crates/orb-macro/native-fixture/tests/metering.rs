//! `metering::orb(..)` in a native host: the Orb imports the meter interface of another Orb,
//! plainly and by key, and natively each imported function calls the provider the host names in
//! `metering::Providers` through the kernel, as a component's import does. Here the provider is
//! `meters`, a native Orb of the same crate; `crates/executor-wasm/tests/orb_macro_native.rs` has a
//! compiled one.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Code, Host, Orb, Provider, Reference, Service, core::Requirement};
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
/// reaches the object of each key, under the principal of the call, whose root holds it: within a
/// call a key's object keeps its total, and the next call finds a new one. The arguments and
/// results cross as the boundary's JSON, a record of the provider's interface included, for a
/// portable caller of the consumer and for a local one, through its client.
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
    let local = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let fed = client.feed(&context, vec![1]).await?;
                let mut at = Vec::new();
                for (key, amount) in [("a", 3), ("a", 4), ("b", 5)] {
                    at.push(
                        client
                            .feed_at(&context, key.into(), reading(amount, 2))
                            .await?,
                    );
                }
                Ok((fed, at))
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(local, (10, vec![Ok(6), Ok(14), Ok(10)]));
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
