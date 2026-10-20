use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{
    ConflictPolicy, ErrorCode, GetOptions, Host, HostOptions, Implementation, Lifetime,
    ManualDriver, Registry, RegistrySubscription, ResourceDeclaration, RuntimeError, ServeOptions,
    contracts::Contract,
};

fn contract(name: &str) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), BTreeMap::new()).unwrap())
}

fn eager() -> ServeOptions {
    ServeOptions {
        lifetime: Lifetime::Host,
        ..Default::default()
    }
}

fn environment() -> (Host, Rc<ManualDriver>) {
    let driver = ManualDriver::new();
    (
        Host::new(
            HostOptions::new("registries", json!("host")),
            driver.clone(),
        ),
        driver,
    )
}

#[test]
fn definition_policy_refusal_preserves_the_bound_empty_view() {
    let (host, driver) = environment();
    let registry = Registry::new("commands", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let before = view.table(None).unwrap();
    assert_eq!(before.code_version(), 1);
    assert!(before.is_empty());
    assert!(Registry::new("", ConflictPolicy::Error).is_err());
    let conflict = Registry::new("commands", ConflictPolicy::Last).unwrap();
    assert_eq!(
        host.registry(&conflict).err().unwrap().reason,
        "registry_definition"
    );
    assert!(view.table(None).unwrap().is_empty());
    driver.run_until(host.close()).unwrap();
    assert_eq!(view.table(None).err().unwrap().reason, "closed");
    assert!(before.is_empty());
}

#[test]
fn host_only_declarations_validate_data_and_use_tuple_keys() {
    let (host, driver) = environment();
    let registry = Registry::new("metadata\0keys", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let registered = registry.clone();
    let factory_view = view.clone();
    host.serve(
        contract("registry.metadata"),
        Rc::new(move |_, _, declaration| {
            assert!(factory_view.table(None)?.is_empty());
            assert_eq!(
                factory_view.subscribe(|_| Ok(())).err().unwrap().code,
                ErrorCode::NotReady
            );
            declaration.register(&registered, "", json!(-0.0))?;
            assert_eq!(
                declaration
                    .register(&registered, "", json!(9))
                    .unwrap_err()
                    .reason,
                "duplicate"
            );
            assert!(
                declaration
                    .register(&registered, "bad", json!(9_007_199_254_740_992_u64))
                    .is_err()
            );
            declaration.register(&registered, "bad", json!({"valid": true}))?;
            declaration.register(&registered, "__proto__", json!({"constructor": null}))?;
            declaration.register(&registered, "a\0b", json!("separator"))?;
            Ok(Implementation::new())
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let table = view.table(None).unwrap();
    assert_eq!(
        table.entries().map(|(key, _)| key).collect::<Vec<_>>(),
        ["", "bad", "__proto__", "a\0b"]
    );
    assert_eq!(table.get("").unwrap().data(), &json!(0));
    assert!(table.get("bad").unwrap().operation().is_none());
    let registered = registry.clone();
    let lazy = contract("registry.referenced");
    host.serve(
        lazy.clone(),
        Rc::new(move |_, _, declaration| {
            assert_eq!(
                declaration
                    .register(&registered, "illegal", Value::Null)
                    .unwrap_err()
                    .reason,
                "not_host_lifetime"
            );
            Ok(Implementation::new())
        }),
        Default::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(lazy, json!({}), GetOptions::default()))
        .unwrap();
    reference.release();
    driver.run_until_stalled();
    assert!(view.table(None).unwrap().get("illegal").is_none());
    assert_eq!(table.len(), 4);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn a_late_conflict_preserves_winner_and_closes_loser_once() {
    let (host, driver) = environment();
    let registry = Registry::new("conflicts", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let open = Rc::new(Cell::new(0));
    let closed = Rc::new(Cell::new(0));
    let (send, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    let winner_registry = registry.clone();
    host.serve(
        contract("registry.winner"),
        Rc::new(move |_, _, declaration| {
            declaration.register(&winner_registry, "shared", json!("winner"))?;
            Ok(Implementation::new())
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let retained = view.table(None).unwrap();
    let owner = retained.get("shared").unwrap().owner_id();
    let received = Rc::new(RefCell::new(Vec::new()));
    let log = received.clone();
    let _subscription = view
        .subscribe(move |table| {
            log.borrow_mut()
                .push(table.get("shared").map(|entry| entry.data().clone()));
            Ok(())
        })
        .unwrap();
    let registered = registry.clone();
    let opened = open.clone();
    let cleaned = closed.clone();
    let loser = contract("registry.loser");
    host.serve(
        loser.clone(),
        Rc::new(move |_, _, declaration| {
            declaration.register(&registered, "unpublished", json!("candidate"))?;
            declaration.register(&registered, "shared", json!("loser"))?;
            let receive = receive.clone();
            let opened = opened.clone();
            let cleaned = cleaned.clone();
            declaration.resource(
                "gate",
                ResourceDeclaration::new(move |_| {
                    let gate = receive.borrow_mut().take().unwrap();
                    let opened = opened.clone();
                    async move {
                        opened.set(opened.get() + 1);
                        gate.await.unwrap();
                        Ok(())
                    }
                    .boxed_local()
                })
                .with_close(move |_| {
                    let cleaned = cleaned.clone();
                    async move {
                        cleaned.set(cleaned.get() + 1);
                        Ok(())
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new())
        }),
        eager(),
    )
    .unwrap();
    let loser_ref = host.hold(loser, json!({}), Default::default()).unwrap();
    driver.run_until_stalled();
    assert_eq!(open.get(), 1);
    assert_eq!(closed.get(), 0);
    assert_eq!(
        view.table(None).unwrap().get("shared").unwrap().owner_id(),
        owner
    );
    send.send(()).unwrap();
    let error = driver.run_until(loser_ref.ready()).unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (ErrorCode::Invalid, "registry_conflict")
    );
    assert_eq!(error.details()["registry"], "conflicts");
    assert_eq!(error.details()["owners"].as_array().unwrap().len(), 2);
    driver.run_until_stalled();
    assert_eq!(closed.get(), 1);
    assert!(view.table(None).unwrap().get("unpublished").is_none());
    assert_eq!(&*received.borrow(), &[Some(json!("winner"))]);
    assert_eq!(retained.get("shared").unwrap().data(), &json!("winner"));
    driver.run_until(host.close()).unwrap();
    assert_eq!(closed.get(), 1);
}

#[test]
fn last_uses_service_declaration_order_and_first_successful_key_order() {
    let (host, driver) = environment();
    let registry = Registry::new("ordered", ConflictPolicy::Last).unwrap();
    let view = host.registry(&registry).unwrap();
    let (send, receive) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(receive)));
    let registered = registry.clone();
    let first = contract("registry.first");
    host.serve(
        first.clone(),
        Rc::new(move |_, _, declaration| {
            declaration.register(&registered, "first", json!(1))?;
            declaration.register(&registered, "shared", json!(1))?;
            let receive = gate.borrow_mut().take().unwrap();
            let receive = Rc::new(RefCell::new(Some(receive)));
            declaration.resource(
                "gate",
                ResourceDeclaration::new(move |_| {
                    let receive = receive.borrow_mut().take().unwrap();
                    async move {
                        receive.await.unwrap();
                        Ok(())
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new())
        }),
        eager(),
    )
    .unwrap();
    let registered = registry.clone();
    let second = contract("registry.second");
    host.serve(
        second.clone(),
        Rc::new(move |_, _, declaration| {
            declaration.register(&registered, "shared", json!(2))?;
            declaration.register(&registered, "second", json!(2))?;
            Ok(Implementation::new())
        }),
        eager(),
    )
    .unwrap();
    let second_ref = driver
        .run_until(host.get(second, json!({}), Default::default()))
        .unwrap();
    let before = view.table(None).unwrap();
    assert_eq!(
        before.entries().map(|(key, _)| key).collect::<Vec<_>>(),
        ["shared", "second"]
    );
    send.send(()).unwrap();
    driver.run_until(host.ready()).unwrap();
    let after = view.table(None).unwrap();
    assert_eq!(
        after.entries().map(|(key, _)| key).collect::<Vec<_>>(),
        ["shared", "second", "first"]
    );
    assert_eq!(after.get("shared").unwrap().data(), &json!(2));
    assert_eq!(before.len(), 2);
    second_ref.release();
    driver.run_until_stalled();
    assert_eq!(
        view.table(None).unwrap().get("shared").unwrap().data(),
        &json!(2)
    );
    host.serve(
        contract("registry.unrelated"),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    assert_eq!(
        view.table(None).unwrap().get("shared").unwrap().data(),
        &json!(2)
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn complete_initial_and_committed_views_allow_reentry_error_and_unsubscribe() {
    let (host, driver) = environment();
    let a = Registry::new("a", ConflictPolicy::Error).unwrap();
    let b = Registry::new("b", ConflictPolicy::Error).unwrap();
    let view_a = host.registry(&a).unwrap();
    let view_b = host.registry(&b).unwrap();
    let provider = contract("registry.coherent");
    let second = Rc::new(RefCell::new(None::<RegistrySubscription>));
    let cancel_second = second.clone();
    let peer = view_b.clone();
    let observing_host = host.clone();
    let observed_contract = provider.clone();
    let events = Rc::new(RefCell::new(Vec::new()));
    let seen = events.clone();
    let _first = view_a
        .subscribe(move |snapshot| {
            seen.borrow_mut().push(snapshot.len());
            if !snapshot.is_empty() {
                assert_eq!(peer.table(None)?.get("entry").unwrap().data(), &json!("b"));
                let reference = observing_host.hold(
                    observed_contract.clone(),
                    json!({}),
                    Default::default(),
                )?;
                assert_eq!(reference.status(), tembra_core::RefStatus::Live);
                reference.release();
                cancel_second.borrow().as_ref().unwrap().unsubscribe();
            }
            Err(RuntimeError::unavailable(
                "observer",
                "observer error is separate from publication",
            ))
        })
        .unwrap();
    let second_calls = Rc::new(Cell::new(0));
    let called = second_calls.clone();
    *second.borrow_mut() = Some(
        view_a
            .subscribe(move |_| {
                called.set(called.get() + 1);
                Ok(())
            })
            .unwrap(),
    );
    assert_eq!(second_calls.get(), 1);
    assert_eq!(
        host.errors()
            .iter()
            .filter(|error| error.reason == "observer")
            .count(),
        1
    );
    host.serve(
        provider,
        Rc::new(move |_, _, declaration| {
            declaration.register(&a, "entry", json!("a"))?;
            declaration.register(&b, "entry", json!("b"))?;
            Ok(Implementation::new())
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    assert_eq!(&*events.borrow(), &[0, 1]);
    assert_eq!(
        second_calls.get(),
        1,
        "removed listener must be skipped within captured publication"
    );
    assert_eq!(
        host.errors()
            .iter()
            .filter(|error| error.reason == "observer")
            .count(),
        2
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn unrelated_per_principal_reference_churn_preserves_table_and_does_not_notify_observers() {
    let (host, driver) = environment();
    let registry = Registry::new("stable-data", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let payload = json!({"blob":"x".repeat(65_536), "nullable":null});
    let metadata = payload.clone();
    host.serve(
        contract("registry.stable-owner"),
        Rc::new(move |_, _, declaration| {
            declaration.register(&registry, "document", metadata.clone())?;
            Ok(Implementation::new())
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let retained = view.table(None).unwrap();
    let owner_id = retained.get("document").unwrap().owner_id();
    let notifications = Rc::new(Cell::new(0));
    let seen = notifications.clone();
    let subscription = view
        .subscribe(move |_| {
            seen.set(seen.get() + 1);
            Ok(())
        })
        .unwrap();
    let caller = contract("registry.unrelated-caller");
    host.serve(
        caller.clone(),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        Default::default(),
    )
    .unwrap();
    for principal in 0..16 {
        let reference = host
            .hold(
                caller.clone(),
                json!({}),
                GetOptions {
                    principal: Some(json!(principal)),
                    ..Default::default()
                },
            )
            .unwrap();
        driver.run_until(reference.ready()).unwrap();
        reference.release();
        driver.run_until_stalled();
    }
    assert_eq!(
        notifications.get(),
        1,
        "unrelated owner lifecycle is not a registry change"
    );
    let current = view.table(None).unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current.get("document").unwrap().owner_id(), owner_id);
    assert_eq!(current.get("document").unwrap().data(), &payload);
    assert_eq!(retained.get("document").unwrap().data(), &payload);
    subscription.unsubscribe();
    driver.run_until(host.close()).unwrap();
}
