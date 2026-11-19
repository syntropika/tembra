//! The memory of `calculator::orb()` in a native host: a `#[resource]` of the Orb's root, which the
//! host opens once, when it installs the Orb, which the methods of every object of the Orb use
//! through `with`, natively as in a component, and which the host closes, through its `#[close]`
//! function, when it removes the Orb. The beads of `abacus::orb()`, a resource of its root too,
//! across a reload: the host keeps them while their version stays the same, also for the beads of
//! `reframed`, of another package version and another Rust type, whose calls it refuses, and
//! closes and opens them again for the beads of `restrung`, at a new version. The gauge of
//! `cistern::orb()`, which depends on its tank, opens again with the tank of `refilled`, at a new
//! version, though its own version stays.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Code, Host, Reference};
use tembra_orb_native_fixture::{
    abacus, calculator, cistern, journal, refilled, reframed, restrung,
};

const ROD: &str = "calculator::example:abacus/rod@1.0.0";
const RESTRUNG_ROD: &str = "calculator::example:abacus/rod@2.0.0";
const REFRAMED_ROD: &str = "calculator::example:abacus/rod@1.1.0";
const METER: &str = "calculator::example:cistern/meter@1.0.0";
const REFILLED_METER: &str = "calculator::example:cistern/meter@2.0.0";

/// Collect what the Orbs of `host` log through their `context`, as the Orb's name and the message:
/// the closure takes what was logged since it was last called.
fn logged(host: &Host) -> impl Fn() -> Vec<String> {
    let records = Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = records.clone();
    host.on_log(move |record| {
        sink.borrow_mut()
            .push(format!("{}: {}", record.orb, record.message));
    });
    move || records.take()
}

/// The register of the Orb named `orb`.
fn register(orb: &str) -> String {
    format!("{orb}::example:calculator/register@1.0.0")
}

/// Add each value to the memory through the register it names, under a root of `host`: what the
/// memory holds after each.
fn add(pool: &mut LocalPool, host: &Host, values: Vec<(Rc<Reference>, i64)>) -> Vec<i64> {
    pool.run_until(host.root("host", move |context| {
        async move {
            let mut held = Vec::new();
            for (register, value) in values {
                held.push(register.call::<_, i64>(&context, "add", (value,)).await?);
            }
            Ok(held)
        }
        .boxed_local()
    }))
    .unwrap()
}

/// The host opens the memory once, when it installs the Orb, before any object exists. The
/// registers of two keys and two principals use the one memory, through a local call, the
/// service's client and a portable call, and the memory stays open between calls; the host closes
/// it when it closes, with what it held last.
#[test]
fn the_memory_opens_once_serves_every_object_and_closes_with_the_orb() {
    journal();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![calculator::orb().unwrap()]).unwrap();
    pool.run_until_stalled();
    assert_eq!(journal(), ["memory open"]);
    let get = |key: &str, principal: &str| {
        Rc::new(
            host.get(&register("calculator"), vec![key.into()], principal)
                .unwrap(),
        )
    };
    let (first, second) = (get("a", "alice"), get("b", "bob"));
    assert_eq!(
        add(
            &mut pool,
            &host,
            vec![(first.clone(), 5), (second.clone(), -2)]
        ),
        [5, 3]
    );
    let client = calculator::register::RegisterClient::new(second.clone());
    let recalled = second.clone();
    let (added, recalled) = pool
        .run_until(host.root("host", move |context| {
            async move {
                let added = client.add(&context, 4).await?;
                let recalled = recalled
                    .call_portable::<_, Value>(&context, "recall", json!([]))
                    .await?;
                Ok((added, recalled))
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!((added, recalled), (7, json!(7)));
    assert!(journal().is_empty());
    first.release().unwrap();
    second.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(journal(), ["memory closed at 7"]);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// Each Orb `orb(..)` builds has its own resources: the crate installed twice, under two names, opens
/// two memories, and each Orb's registers use their own, where a component's would live in each of
/// two instances.
#[test]
fn each_orb_has_its_own_memory() {
    journal();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        calculator::orb_named("left", 1).unwrap(),
        calculator::orb_named("right", 1).unwrap(),
    ])
    .unwrap();
    pool.run_until_stalled();
    assert_eq!(journal(), ["memory open", "memory open"]);
    let get = |orb: &str| {
        Rc::new(
            host.get(&register(orb), vec!["main".into()], "host")
                .unwrap(),
        )
    };
    let (left, right) = (get("left"), get("right"));
    assert_eq!(
        add(
            &mut pool,
            &host,
            vec![(left.clone(), 2), (right.clone(), 10), (left.clone(), 3)]
        ),
        [2, 10, 5]
    );
    left.release().unwrap();
    right.release().unwrap();
    pool.run_until(host.close()).unwrap();
    let mut closed = journal();
    closed.sort();
    assert_eq!(closed, ["memory closed at 10", "memory closed at 5"]);
}

/// Slide the beads by `by` through the rod, under a root of `host`: how far they were slid in all.
fn slide(pool: &mut LocalPool, host: &Host, rod: &Rc<Reference>, by: i64) -> i64 {
    let rod = rod.clone();
    pool.run_until(host.root("host", move |context| {
        async move { rod.call::<_, i64>(&context, "slide", (by,)).await }.boxed_local()
    }))
    .unwrap()
}

/// A reload keeps the beads, a resource of the Orb's root, while their name and version stay the
/// same, as the host keeps the resources of any native Orb: nothing closes or opens, and the new
/// code's method, through the same reference, slides the value the old code slid. The host closes
/// that value once, when it closes, through the closer of the revision that declared it, which
/// still logs for the Orb through its `context`, though the kernel has forgotten that revision. A
/// component would open its beads again, in a fresh instance.
#[test]
fn a_reload_keeps_a_root_resource_whose_version_stays() {
    journal();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let logged = logged(&host);
    host.install(vec![abacus::orb().unwrap()]).unwrap();
    pool.run_until_stalled();
    assert_eq!(journal(), ["beads 1 open"]);
    assert_eq!(logged(), ["calculator: beads 1 open"]);
    let rod = Rc::new(host.get(ROD, vec!["main".into()], "host").unwrap());
    assert_eq!(slide(&mut pool, &host, &rod, 5), 5);
    pool.run_until(host.reload(vec![abacus::orb().unwrap()], 5_000))
        .unwrap();
    pool.run_until(host.settled());
    assert!(journal().is_empty());
    assert!(logged().is_empty());
    assert_eq!(slide(&mut pool, &host, &rod, 1), 6);
    rod.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(journal(), ["beads 1 closed at 6"]);
    assert_eq!(logged(), ["calculator: beads 1 closed at 6"]);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A reload to code whose beads are at a new version, `#[resource(version = 2)]`, opens the new
/// code's beads with their `#[open]` function, then closes the beads the old code slid, once,
/// through theirs. The new code's rod, of its interface at 2.0.0, which the host acquires once it
/// has released the old one, slides the new beads from zero. The host closes the new beads when it
/// closes, and never the old ones again. Both `#[open]` and `#[close]` log for the Orb through
/// their `context`: the new beads open before the kernel commits the new revision, the old ones
/// close once it has forgotten the old one.
#[test]
fn a_reload_reopens_a_root_resource_at_a_new_version() {
    journal();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let logged = logged(&host);
    host.install(vec![abacus::orb().unwrap()]).unwrap();
    let rod = Rc::new(host.get(ROD, vec!["main".into()], "host").unwrap());
    assert_eq!(slide(&mut pool, &host, &rod, 5), 5);
    rod.release().unwrap();
    pool.run_until_stalled();
    assert_eq!(journal(), ["beads 1 open"]);
    assert_eq!(logged(), ["calculator: beads 1 open"]);
    pool.run_until(host.reload(vec![restrung::orb().unwrap()], 5_000))
        .unwrap();
    pool.run_until(host.settled());
    assert_eq!(journal(), ["beads 2 open", "beads 1 closed at 5"]);
    assert_eq!(
        logged(),
        [
            "calculator: beads 2 open",
            "calculator: beads 1 closed at 5"
        ]
    );
    let rod = Rc::new(host.get(RESTRUNG_ROD, vec!["main".into()], "host").unwrap());
    assert_eq!(slide(&mut pool, &host, &rod, 1), 1);
    rod.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(journal(), ["beads 2 closed at 1"]);
    assert_eq!(logged(), ["calculator: beads 2 closed at 1"]);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A reload to code of another package version whose beads stay at version 1 keeps them, as the
/// resource's name and version decide, not the package's, which `restrung` changes too: nothing
/// opens or closes. The host closes the beads the old code slid once, when it closes, through the
/// old code's `#[close]` function, and never opens the new code's.
#[test]
fn a_reload_to_another_package_version_keeps_a_root_resource_whose_version_stays() {
    journal();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![abacus::orb().unwrap()]).unwrap();
    let rod = Rc::new(host.get(ROD, vec!["main".into()], "host").unwrap());
    assert_eq!(slide(&mut pool, &host, &rod, 5), 5);
    rod.release().unwrap();
    pool.run_until_stalled();
    assert_eq!(journal(), ["beads 1 open"]);
    pool.run_until(host.reload(vec![reframed::orb().unwrap()], 5_000))
        .unwrap();
    pool.run_until(host.settled());
    assert!(journal().is_empty());
    pool.run_until(host.close()).unwrap();
    assert_eq!(journal(), ["beads 1 closed at 5"]);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// The beads the host keeps from `abacus` are another Rust type than the beads of `reframed`, as
/// any other module's are, even with the same fields: a call of the new code's rod, local or
/// portable, answers `invalid/rust_type` without a panic, as the runtime answers a hand-written
/// Orb that reads the resource as its type, and leaves the kept beads as they were.
#[test]
fn a_call_reaching_a_kept_root_resource_of_another_type_answers_rust_type() {
    journal();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![abacus::orb().unwrap()]).unwrap();
    let rod = Rc::new(host.get(ROD, vec!["main".into()], "host").unwrap());
    assert_eq!(slide(&mut pool, &host, &rod, 5), 5);
    rod.release().unwrap();
    pool.run_until(host.reload(vec![reframed::orb().unwrap()], 5_000))
        .unwrap();
    pool.run_until(host.settled());
    let rod = Rc::new(host.get(REFRAMED_ROD, vec!["main".into()], "host").unwrap());
    let called = rod.clone();
    let (local, portable) = pool
        .run_until(host.root("host", move |context| {
            async move {
                let local = called.call::<_, i64>(&context, "slide", (1i64,)).await;
                let portable = called
                    .call_portable::<_, Value>(&context, "slide", json!([1]))
                    .await;
                Ok((local, portable))
            }
            .boxed_local()
        }))
        .unwrap();
    for error in [local.unwrap_err(), portable.unwrap_err()] {
        assert_eq!(
            (error.code, error.reason.as_str()),
            (Code::Invalid, "rust_type"),
            "{error}"
        );
    }
    rod.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(journal(), ["beads 1 open", "beads 1 closed at 5"]);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// A reload to code whose tank is at a new version reopens the gauge too, though the gauge's own
/// version stays 1: the kernel keeps a resource only when its dependencies are the ones it opened
/// after, and the new tank is another. The new gauge reads the new tank; the old gauge closes
/// before the old tank.
#[test]
fn a_reload_reopens_a_resource_whose_dependency_reopens() {
    journal();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![cistern::orb().unwrap()]).unwrap();
    let meter = Rc::new(host.get(METER, vec!["main".into()], "host").unwrap());
    assert_eq!(read(&mut pool, &host, &meter), 1);
    meter.release().unwrap();
    pool.run_until_stalled();
    assert_eq!(journal(), ["tank 1 open", "gauge 1 open at 1"]);
    pool.run_until(host.reload(vec![refilled::orb().unwrap()], 5_000))
        .unwrap();
    pool.run_until(host.settled());
    assert_eq!(
        journal(),
        [
            "tank 2 open",
            "gauge 1 open at 2",
            "gauge 1 closed at 1",
            "tank 1 closed"
        ]
    );
    let meter = Rc::new(
        host.get(REFILLED_METER, vec!["main".into()], "host")
            .unwrap(),
    );
    assert_eq!(read(&mut pool, &host, &meter), 2);
    meter.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(journal(), ["gauge 1 closed at 2", "tank 2 closed"]);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}

/// Read the gauge through the meter, under a root of `host`.
fn read(pool: &mut LocalPool, host: &Host, meter: &Rc<Reference>) -> i64 {
    let meter = meter.clone();
    pool.run_until(host.root("host", move |context| {
        async move { meter.call::<_, i64>(&context, "read", ()).await }.boxed_local()
    }))
    .unwrap()
}
