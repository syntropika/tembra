//! What the native tests of the selected module share: a host on a local executor, and calls of a
//! service's object with the boundary's JSON, as a component's callers make them.
#![allow(dead_code, reason = "each module's tests use some of these")]

// The facade by the name the selected module's crate gives it.
#[cfg(feature = "native-sdk")]
pub use ::sdk as facade;
#[cfg(not(feature = "native-sdk"))]
pub use ::tembra as facade;

use facade::{Host, Orb, Reference, core::Code};
use futures::{FutureExt, executor::LocalPool};
use serde_json::Value;
use std::rc::Rc;

/// The name `orb()` gives the Orb natively: the crate's, as the manifest names none.
pub const ORB: &str = "tembra-orb-runtime-fixture";

/// A host on a local executor, with the Orb installed.
pub fn hosting(orb: Orb) -> (LocalPool, Host) {
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    (pool, host)
}

/// A reference to the `main` object of the Orb's service `interface` of `alice`, once it is ready.
pub fn ready(pool: &mut LocalPool, host: &Host, interface: &str) -> Rc<Reference> {
    let name = format!("{ORB}::{interface}");
    let reference = Rc::new(host.get(&name, vec!["main".into()], "alice").unwrap());
    pool.run_until(reference.ready()).unwrap();
    reference
}

/// Call `method` of the object portably, with the boundary's JSON, in a root of `principal`.
pub fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    principal: &str,
    method: &'static str,
    arguments: Value,
) -> Value {
    let reference = reference.clone();
    pool.run_until(host.root(principal, move |context| {
        async move { reference.call_portable(&context, method, arguments).await }.boxed_local()
    }))
    .unwrap()
}

/// The message of the error a native host gets for the Orb `orb()` built: `unsupported/native`,
/// naming what the native side does not cover.
pub fn refused(orb: facade::Result<Orb>) -> String {
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let error = orb
        .and_then(|orb| host.install(vec![orb]))
        .expect_err("the native side does not build the Orb");
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Unsupported, "native"),
        "{error}"
    );
    error.message
}
