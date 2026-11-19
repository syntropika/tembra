//! `pier::orb(..)` in a native host: the create of `Boat` builds each object from the host's
//! inputs, and the factory declares the object's bollard, a resource of the service's module, after
//! it; the host opens the bollard with the object, and a method borrowing `&self` reaches the
//! object and its bollard.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Reference};
use tembra_orb_native_fixture::pier;

const BOAT: &str = "calculator::example:pier/boat@1.0.0";

/// Call `tie` of a boat portably, in a root of `alice`.
fn tie(pool: &mut LocalPool, host: &Host, boat: &Rc<Reference>, ropes: u32) -> Value {
    let boat = boat.clone();
    pool.run_until(host.root("alice", move |context| {
        async move { boat.call_portable(&context, "tie", json!([ropes])).await }.boxed_local()
    }))
    .unwrap()
}

#[test]
fn an_object_a_create_builds_owns_a_resource() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        pier::orb(pier::Inputs {
            port: "Genoa".into(),
        })
        .unwrap(),
    ])
    .unwrap();
    let boat = |pool: &mut LocalPool, key: &str| {
        let boat = Rc::new(host.get(BOAT, vec![key.into()], "alice").unwrap());
        pool.run_until(boat.ready()).unwrap();
        boat
    };
    let one = boat(&mut pool, "one");
    let two = boat(&mut pool, "two");
    assert_eq!(tie(&mut pool, &host, &one, 2), json!("Genoa one: 2 ropes"));
    assert_eq!(tie(&mut pool, &host, &one, 1), json!("Genoa one: 3 ropes"));
    assert_eq!(tie(&mut pool, &host, &two, 4), json!("Genoa two: 4 ropes"));
    one.release().unwrap();
    two.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
