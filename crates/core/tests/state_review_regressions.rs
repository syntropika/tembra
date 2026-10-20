use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use tembra_core::{
    Cell, GetOptions, Host, HostOptions, Implementation, ManualDriver, RefStatus, ReplaceOptions,
    ServeOptions, StateDeclaration, StateEvent,
    contracts::{Contract, Member},
};

fn contract(name: &str, members: &[&str]) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            1,
            json!({}),
            members
                .iter()
                .map(|name| {
                    (
                        (*name).into(),
                        Member::State {
                            schema: json!({"type":"integer"}),
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        )
        .unwrap(),
    )
}

fn host() -> (Host, Rc<ManualDriver>) {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("review", json!("host")), driver.clone());
    (host, driver)
}

#[test]
fn forwarding_two_external_sources_keeps_latest_snapshot_across_hydration_wait() {
    let (host, driver) = host();
    let first_contract = contract("review.external.first", &["value"]);
    let saved = Rc::new(RefCell::new(None::<Cell>));
    let captured = saved.clone();
    host.serve(
        first_contract.clone(),
        Rc::new(move |_, _, declaration| {
            let value = declaration.state(
                "value",
                StateDeclaration::new(json!({"type":"integer"}), json!(1)),
            )?;
            *captured.borrow_mut() = Some(value.clone());
            Ok(Implementation::new().state("value", value.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let first = driver
        .run_until(host.get(first_contract, json!({}), GetOptions::default()))
        .unwrap();
    let second_contract = contract("review.external.second", &["value"]);
    let (complete, pending) = oneshot::channel();
    let pending = Rc::new(RefCell::new(Some(pending)));
    host.serve(
        second_contract.clone(),
        Rc::new(move |_, _, declaration| {
            let pending = pending.clone();
            let value = declaration.state(
                "value",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |_| {
                    let pending = pending.borrow_mut().take().unwrap();
                    async move {
                        pending.await.unwrap();
                        Ok(json!(10))
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new().state("value", value.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let second = host
        .hold(second_contract, json!({}), GetOptions::default())
        .unwrap();
    let forwarding_contract = contract("review.external.forward", &["first", "second", "sum"]);
    let (first_factory, second_factory) = (first.clone(), second.clone());
    host.serve(
        forwarding_contract.clone(),
        Rc::new(move |_, _, declaration| {
            let first = first_factory.readable("value")?;
            let second = second_factory.readable("value")?;
            let sum = declaration.derive(
                vec![first.clone(), second.clone()],
                json!({"type":"integer"}),
                |values| {
                    Ok(json!(
                        values[0].value().as_i64().unwrap() + values[1].value().as_i64().unwrap()
                    ))
                },
            )?;
            Ok(Implementation::new()
                .state("first", first)
                .state("second", second)
                .state("sum", sum))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let forwarding = host
        .hold(forwarding_contract, json!({}), GetOptions::default())
        .unwrap();
    let observed = Rc::new(RefCell::new(Vec::new()));
    let events = observed.clone();
    let _subscription = forwarding
        .readable("first")
        .unwrap()
        .subscribe_events(move |event| {
            events.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(forwarding.status(), RefStatus::Loading);
    assert!(observed.borrow().is_empty());
    let first_cell = saved.borrow().as_ref().unwrap().clone();
    driver
        .run_until(host.root(move |ctx| {
            async move {
                first_cell.replace(&ctx, json!(2), ReplaceOptions::default())?;
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(
        first.readable("value").unwrap().snapshot().unwrap().value(),
        &json!(2)
    );
    assert_eq!(forwarding.status(), RefStatus::Loading);
    complete.send(()).unwrap();
    driver.run_until(forwarding.ready()).unwrap();
    assert_eq!(
        forwarding
            .readable("first")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(2)
    );
    assert_eq!(
        forwarding
            .readable("second")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(10)
    );
    assert_eq!(
        forwarding
            .readable("sum")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(12)
    );
    let events = observed.borrow();
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], StateEvent::Snapshot(_)));
    assert_eq!(events[0].snapshot().value(), &json!(2));
    drop(events);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn direct_observer_subscribed_after_silent_initialization_receives_activation_write() {
    let (host, driver) = host();
    let contract = contract("review.activation.observer", &["value", "blocker"]);
    let (complete, pending) = oneshot::channel();
    let pending = Rc::new(RefCell::new(Some(pending)));
    let observed = Rc::new(RefCell::new(Vec::<Value>::new()));
    let seen = observed.clone();
    let saved = Rc::new(RefCell::new(None::<Cell>));
    let captured = saved.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let value = declaration.state(
                "value",
                StateDeclaration::with_initializer(json!({"type":"integer"}), |_| {
                    async { Ok(json!(1)) }.boxed_local()
                }),
            )?;
            *captured.borrow_mut() = Some(value.clone());
            let (value_initializer, pending, seen) = (value.clone(), pending.clone(), seen.clone());
            let blocker = declaration.state(
                "blocker",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |ctx| {
                    let pending = pending.borrow_mut().take().unwrap();
                    let (value, seen) = (value_initializer.clone(), seen.clone());
                    async move {
                        pending.await.unwrap();
                        assert_eq!(value.snapshot()?.value(), &json!(1));
                        let _subscription = value.readable().subscribe(move |snapshot| {
                            seen.borrow_mut().push(snapshot.value().clone());
                            Ok(())
                        })?;
                        value.replace(&ctx, json!(2), ReplaceOptions::default())?;
                        assert_eq!(value.snapshot()?.value(), &json!(2));
                        Ok(json!(0))
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new()
                .state("value", value.readable())
                .state("blocker", blocker.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Loading);
    assert_eq!(
        saved.borrow().as_ref().unwrap().snapshot().unwrap().value(),
        &json!(1)
    );
    assert!(observed.borrow().is_empty());
    complete.send(()).unwrap();
    driver.run_until(reference.ready()).unwrap();
    assert_eq!(
        reference
            .readable("value")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(2)
    );
    assert_eq!(&*observed.borrow(), &[json!(1), json!(2)]);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn immediate_cell_can_change_while_its_derived_sibling_waits_for_initialization() {
    let (host, driver) = host();
    let contract = contract(
        "review.activation.pending",
        &["immediate", "pending", "sum"],
    );
    let (complete, pending) = oneshot::channel();
    let pending = Rc::new(RefCell::new(Some(pending)));
    let saved = Rc::new(RefCell::new(None::<Cell>));
    let captured = saved.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let immediate = declaration.state(
                "immediate",
                StateDeclaration::new(json!({"type":"integer"}), json!(1)),
            )?;
            *captured.borrow_mut() = Some(immediate.clone());
            let pending = pending.clone();
            let initialized = declaration.state(
                "pending",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |_| {
                    let pending = pending.borrow_mut().take().unwrap();
                    async move {
                        pending.await.unwrap();
                        Ok(json!(10))
                    }
                    .boxed_local()
                }),
            )?;
            let sum = declaration.derive(
                vec![immediate.readable(), initialized.readable()],
                json!({"type":"integer"}),
                |values| {
                    Ok(json!(
                        values[0].value().as_i64().unwrap() + values[1].value().as_i64().unwrap()
                    ))
                },
            )?;
            Ok(Implementation::new()
                .state("immediate", immediate.readable())
                .state("pending", initialized.readable())
                .state("sum", sum))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Loading);
    let immediate = saved.borrow().as_ref().unwrap().clone();
    let direct_events = Rc::new(RefCell::new(Vec::new()));
    let events = direct_events.clone();
    let _subscription = immediate
        .readable()
        .subscribe(move |snapshot| {
            events.borrow_mut().push(snapshot.value().clone());
            Ok(())
        })
        .unwrap();
    driver
        .run_until(host.root(move |ctx| {
            async move {
                immediate.replace(&ctx, json!(3), ReplaceOptions::default())?;
                Ok(())
            }
            .boxed_local()
        }))
        .expect("an initialized cell remains writable while a derived dependency is pending");
    assert_eq!(&*direct_events.borrow(), &[json!(1), json!(3)]);
    assert_eq!(reference.status(), RefStatus::Loading);
    assert!(reference.readable("sum").unwrap().snapshot().is_err());
    complete.send(()).unwrap();
    driver.run_until(reference.ready()).unwrap();
    assert_eq!(
        reference
            .readable("immediate")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(3)
    );
    assert_eq!(
        reference
            .readable("sum")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(13)
    );
    driver.run_until(host.close()).unwrap();
}
