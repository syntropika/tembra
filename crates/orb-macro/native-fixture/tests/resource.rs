//! The memory of `calculator::orb()` in a native host: a `#[resource]` of the Orb's root, which the
//! host opens once, when it installs the Orb, which the methods of every object of the Orb use
//! through `with`, natively as in a component, and which the host closes, through its `#[close]`
//! function, when it removes the Orb.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Reference};
use tembra_orb_native_fixture::{calculator, journal};

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
    let client = calculator::RegisterClient::new(second.clone());
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
