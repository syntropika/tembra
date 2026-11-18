//! `burrow::orb()` and `warren::orb()` in a native host: a service in a private module, whose WIT
//! resource sits in another module, installs natively when the resource's module needs nothing of
//! the service's client, and when it can name the client.
use futures::{FutureExt, executor::LocalPool};
use std::rc::Rc;
use tembra::{Host, Orb};
use tembra_orb_native_fixture::{burrow, warren};

/// Install `orb` and call the method `kinds` of its service `interface`.
fn kinds(orb: Orb, interface: &str) -> u32 {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let tokens = Rc::new(host.get(interface, vec!["main".into()], "alice").unwrap());
    let called = tokens.clone();
    let kinds = pool
        .run_until(host.root("alice", move |context| {
            async move { called.call::<_, u32>(&context, "kinds", ()).await }.boxed_local()
        }))
        .unwrap();
    tokens.release().unwrap();
    pool.run_until(host.close()).unwrap();
    kinds
}

/// The resource has no constructor and no static function, so the module `tags` needs nothing of
/// the client of `Tokens`, which it could not name: the Orb installs.
#[test]
fn a_resource_without_constructor_beside_a_private_service_installs() {
    assert_eq!(
        kinds(
            burrow::orb().unwrap(),
            "calculator::example:burrow/tokens@1.0.0"
        ),
        1
    );
}

/// The resource has a constructor, which the client of `Tokens` takes in the module `tags`: the
/// module `nest` and the service are `pub(crate)`, so `tags` names the client, and the Orb
/// installs.
#[test]
fn a_resource_with_a_constructor_beside_a_crate_visible_service_installs() {
    assert_eq!(
        kinds(
            warren::orb().unwrap(),
            "calculator::example:warren/tokens@1.0.0"
        ),
        1
    );
}
