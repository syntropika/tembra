//! Canonical packages cross real macro-generated provider and consumer boundaries.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Host, Provider};
use tembra_orb_native_fixture::canonical_packages::{consumer, provider};

#[test]
fn canonical_capability_packages_compose_without_schema_copies() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        provider::orb_named("model", 1).unwrap(),
        consumer::orb_named(
            "session",
            1,
            consumer::Providers {
                agent: Provider::new("model", 1),
            },
        )
        .unwrap(),
    ])
    .unwrap();
    let reference = Rc::new(
        host.get(
            "session::example:canonical-consumer/api@1.0.0",
            vec!["main".into()],
            "alice",
        )
        .unwrap(),
    );
    pool.run_until(reference.ready()).unwrap();
    let call = reference.clone();
    let output: Value = pool
        .run_until(host.root("alice", move |context| {
            async move {
                call.call_portable(&context, "describe", Vec::<Value>::new())
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(
        output,
        json!({ "model": {
            "metadata": { "context": { "principal": "alice" }, "name": "model" },
            "tools": [{ "context": { "principal": "alice" }, "name": "browser" }]
        }})
    );
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
