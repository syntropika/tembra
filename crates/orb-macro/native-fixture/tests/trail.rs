//! `trail::orb(..)` in a native host: the Orb registers a service and runs a root of a module the
//! Orb module cannot name, and calls a service's method named as a method of `Rc` is, as the
//! service's.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, contracts::Contract};
use tembra_orb_native_fixture::trail;

const WALKER: &str = "calculator::example:trail/walker@1.0.0";
const GUIDE: &str = "calculator::example:trail/guide@1.0.0";

/// The Orb of the trail named `name`.
fn orb(name: &str) -> tembra::Orb {
    trail::orb(trail::Inputs {
        name: name.to_owned(),
    })
    .unwrap()
}

/// The walker sits two private modules below the Orb module: the Orb declares it, as the plan of
/// the module's contract does, and its method answers.
#[test]
fn a_service_two_private_modules_down_installs() {
    let orb = orb("the ridge");
    let expected = Contract::from_source("trail.wit", trail::WIT, Some("trail"))
        .unwrap()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    assert_eq!(orb.declaration(), &expected);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let walker = Rc::new(host.get(WALKER, vec!["main".into()], "alice").unwrap());
    let called = walker.clone();
    let steps = pool
        .run_until(host.root("alice", move |context| {
            async move { called.call::<_, u32>(&context, "steps", ()).await }.boxed_local()
        }))
        .unwrap();
    assert_eq!(steps, 1);
    walker.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// The Orb's root sits two private modules below the Orb module too: the Orb runs it as the host
/// installs it, and a method of the walker reads its value, a leg per word of the trail's name.
#[test]
fn a_root_two_private_modules_down_installs() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb("the ridge")]).unwrap();
    let walker = Rc::new(host.get(WALKER, vec!["main".into()], "alice").unwrap());
    let called = walker.clone();
    let legs = pool
        .run_until(host.root("alice", move |context| {
            async move { called.call::<_, u32>(&context, "legs", ()).await }.boxed_local()
        }))
        .unwrap();
    assert_eq!(legs, 2);
    walker.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

/// The guide's `clone` borrows `&self`, and each guide object is kept in an `Rc`, which has a
/// `clone` of its own: the call is the guide's, through the client and portably.
#[test]
fn a_method_named_clone_is_the_services() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb("the ridge")]).unwrap();
    let guide = Rc::new(host.get(GUIDE, vec!["main".into()], "alice").unwrap());
    let client = trail::GuideClient::new(guide.clone());
    let called = guide.clone();
    let (local, portable) = pool
        .run_until(host.root("alice", move |context| {
            async move {
                let local = client.clone(&context).await?;
                let portable = called
                    .call_portable::<_, Value>(&context, "clone", json!([]))
                    .await?;
                Ok((local, portable))
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(local, "a copy of the ridge");
    assert_eq!(portable, "a copy of the ridge");
    guide.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
