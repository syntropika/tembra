//! The service policies of a compiled artifact give its services the ownership policy a source
//! bundle declares in executable code: the kernel pins host-lifetime objects and lingers referenced ones.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, core::Lifetime};
use tembra_contracts::{ComponentRoles, ServicePolicy};
use tembra_executor_wasm::{Engine, Install, Limits};

#[path = "support/components.rs"]
mod components;
use components::*;

/// The kernel service of the compiled provider named `math`.
const SERVICE: &str = "math::example:deps/math@1.0.0";

/// Install the provider with one service policy and read what the kernel declared for it. The
/// kernel admits a host-lifetime object only under key `main` and the host principal, so the
/// caller names the principal.
fn declared(policy: ServicePolicy, principal: &'static str) -> (tembra::core::Service, Value) {
    let roles = ComponentRoles {
        services: [(MATH.to_owned(), policy)].into(),
        ..Default::default()
    };
    let engine = Engine::new(Limits::default()).unwrap();
    let provider = engine
        .load(
            &artifact_with("math-provider", MATH_PROVIDER, "math", &roles),
            Install::default(),
        )
        .unwrap();
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![provider]).unwrap();
    let service = host
        .kernel()
        .installed_orbs()
        .into_iter()
        .flat_map(|orb| orb.declaration.services)
        .find(|service| service.name == SERVICE)
        .expect("the compiled service is installed");
    // The policy changes ownership, never the calls.
    let reference = Rc::new(host.get(SERVICE, vec!["main".into()], principal).unwrap());
    pool.run_until(reference.ready()).unwrap();
    let called = reference.clone();
    let doubled = pool
        .run_until(host.root(principal, move |context| {
            async move {
                called
                    .call::<_, Value>(&context, "double", json!([21]))
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    (service, doubled)
}

/// Without a policy a compiled service's objects live while referenced, as before.
#[test]
fn compiled_services_are_referenced_by_default() {
    let (service, doubled) = declared(ServicePolicy::default(), "alice");
    assert_eq!(service.lifetime, Lifetime::Referenced);
    assert_eq!(service.linger_ms, 0);
    assert_eq!(doubled, 42);
}

/// A service policy's lifetime and linger reach the kernel's declaration of the service.
#[test]
fn service_policies_pin_or_linger_compiled_service_objects() {
    let (pinned, doubled) = declared(
        ServicePolicy {
            lifetime: tembra_contracts::Lifetime::Host,
            linger_ms: 0,
        },
        "host",
    );
    assert_eq!(pinned.lifetime, Lifetime::Host);
    assert_eq!(doubled, 42);
    let (lingering, _) = declared(
        ServicePolicy {
            lifetime: tembra_contracts::Lifetime::Referenced,
            linger_ms: 2_500,
        },
        "alice",
    );
    assert_eq!(lingering.lifetime, Lifetime::Referenced);
    assert_eq!(lingering.linger_ms, 2_500);
}
