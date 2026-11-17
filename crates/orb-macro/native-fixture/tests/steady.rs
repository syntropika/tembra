//! `steady::orb()` in a native host: the module has a state, a resource of the Orb's root and the
//! `context`, and implements an interface of a contract whose `deps/` holds the kernel's package,
//! as `cargo tembra build` writes it.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Reference};
use tembra_orb_native_fixture::steady;

const COUNTER: &str = "calculator::example:counter/counter@1.0.0";

/// Call `method` of the counter portably, under a root of `principal`.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    counter: &Rc<Reference>,
    principal: &str,
    method: &'static str,
    arguments: Value,
) -> Value {
    let counter = counter.clone();
    pool.run_until(host.root(principal, move |context| {
        async move { counter.call_portable(&context, method, arguments).await }.boxed_local()
    }))
    .unwrap()
}

/// The macro reads the kernel's package once, from the consumed contract, and the Orb answers:
/// each object counts in its state, through the root's resource, and the `context` names the
/// call's principal, which the counter refuses for `mallory`.
#[test]
fn a_contract_holding_the_kernel_package_is_consumed() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![steady::orb().unwrap()]).unwrap();
    let counter = Rc::new(host.get(COUNTER, vec!["main".into()], "alice").unwrap());
    pool.run_until(counter.ready()).unwrap();
    let mut add =
        |principal: &str, by: i64| call(&mut pool, &host, &counter, principal, "add", json!([by]));
    assert_eq!(
        add("alice", 2),
        json!({"tag": "ok", "val": {"total": 2, "steps": 1}})
    );
    assert_eq!(
        add("alice", 3),
        json!({"tag": "ok", "val": {"total": 5, "steps": 2}})
    );
    assert_eq!(
        add("mallory", 1),
        json!({"tag": "err", "val": "mallory may not count"})
    );
    assert_eq!(
        call(&mut pool, &host, &counter, "alice", "read", json!([])),
        json!({"total": 5, "steps": 2})
    );
    assert_eq!(
        *counter.state::<Value>().unwrap().value().unwrap(),
        json!({"count": {"total": 5, "steps": 2}})
    );
    counter.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
