//! `filter::orb(..)` in a native host: the Orb contributes to the hook of a gate, which
//! `filter::Providers` names. Here the gate is a native Orb written by hand, which declares the
//! hook with the signature of its WIT; `crates/executor-wasm/tests/orb_macro_native.rs` has a
//! compiled one.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::{Hook, Host, Orb, Provider, Service, contracts::Contract, core::Requirement};
use tembra_orb_native_fixture::filter;

const GUARD: &str = "example:gate/guard@1.0.0";
const ADMISSIONS: &str = "gate::example:gate/admissions@1.0.0";

/// The signature of the hook's callback, as the gate's contract gives it.
fn signature() -> String {
    let source = format!(
        "{}\nworld gate {{ import guard; export guard; export admissions; }}\n",
        include_str!("../wit/deps/gate/gate.wit")
    );
    Contract::from_source("gate.wit", &source, Some("gate"))
        .unwrap()
        .function_signature(GUARD, "apply", true)
        .unwrap()
}

/// A gate written by hand: it declares the hook, a waterfall of the admission's JSON, and its
/// service runs the hook over a fresh admission of each value.
fn gate() -> Orb {
    let hook = Hook::<Value, Value>::waterfall(GUARD).signature(signature());
    let declared = hook.clone();
    Orb::new("gate", 1).hook(hook).service(
        Service::new(ADMISSIONS, move |declaration| declaration.hook(&declared)).method(
            "admit",
            |hook, context, (value,): (i32,)| async move {
                hook.run(&context, json!({ "value": value, "blocked": false }))
                    .await
            },
        ),
    )
}

/// The Orb requires the hook of the gate with the signature of its callback, and its
/// contribution runs in the gate's plan: it reads the admission with its codec, in the root of the
/// contributor, under the principal of the call, and its answer is the plan's.
#[test]
fn a_native_contribution_joins_a_native_hook() {
    let contributor = filter::orb(filter::Providers {
        guard: Provider::new("gate", 1),
    })
    .unwrap();
    assert_eq!(
        contributor.declaration().requires,
        [Requirement {
            signatures: Default::default(),
            hook_signatures: [(GUARD.to_owned(), signature())].into(),
            orb: "gate".into(),
            api_version: 1,
            methods: Default::default(),
            hooks: [GUARD.to_owned()].into(),
        }]
    );
    assert!(contributor.declaration().services.is_empty());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![gate(), contributor]).unwrap();
    let admissions = Rc::new(host.get(ADMISSIONS, vec!["main".into()], "alice").unwrap());
    pool.run_until(admissions.ready()).unwrap();
    filter::seen();
    for (value, admitted) in [
        (2, json!({ "value": 2, "blocked": false })),
        (-1, json!({ "value": -1, "blocked": true })),
    ] {
        let called = admissions.clone();
        let answer = pool
            .run_until(host.root("alice", move |context| {
                async move { called.call::<_, Value>(&context, "admit", (value,)).await }
                    .boxed_local()
            }))
            .unwrap();
        assert_eq!(answer, admitted, "{value}");
    }
    assert_eq!(
        filter::seen(),
        ["2 for alice on []@host", "-1 for alice on []@host"]
    );
    admissions.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
