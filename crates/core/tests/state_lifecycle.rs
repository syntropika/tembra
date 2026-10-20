use std::{
    cell::{Cell as Counter, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

fn host() -> (Host, Rc<ManualDriver>) {
    let driver = ManualDriver::new();
    (
        Host::new(
            HostOptions::new("state-tests", json!("host")),
            driver.clone(),
        ),
        driver,
    )
}

fn contract(name: &str, version: u64, states: &[(&str, Value)], methods: &[&str]) -> Rc<Contract> {
    let mut members: BTreeMap<_, _> = states
        .iter()
        .map(|(name, schema)| {
            (
                (*name).into(),
                Member::State {
                    schema: schema.clone(),
                },
            )
        })
        .collect();
    for name in methods {
        members.insert(
            (*name).into(),
            Member::Method {
                args: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                result: None,
            },
        );
    }
    Rc::new(Contract::local(name, version, json!({}), members).unwrap())
}

#[test]
fn initializers_use_open_resources_independent_roots_and_completed_sibling_values() {
    let (host, driver) = host();
    let contract = contract(
        "initializers",
        1,
        &[
            ("immediate", json!({"type":"integer"})),
            ("first", json!({"type":"integer"})),
            ("second", json!({"type":"integer"})),
            ("sum", json!({"type":"integer"})),
        ],
        &[],
    );
    let (start, wait_start) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(wait_start)));
    let (finished, wait_finished) = oneshot::channel();
    let finished = Rc::new(RefCell::new(Some(finished)));
    let wait_finished = Rc::new(RefCell::new(Some(wait_finished)));
    let roots = Rc::new(RefCell::new(Vec::new()));
    let observed_roots = roots.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let resource = declaration.resource(
                "ready",
                ResourceDeclaration::new(|_| async { Ok(7) }.boxed_local()),
            )?;
            let immediate = declaration.state(
                "immediate",
                StateDeclaration::new(json!({"type":"integer"}), json!(0)),
            )?;
            let (gate, finished, roots) = (gate.clone(), finished.clone(), observed_roots.clone());
            let first_immediate = immediate.clone();
            let first_roots = roots.clone();
            let first = declaration.state(
                "first",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |ctx| {
                    let receive = gate.borrow_mut().take().unwrap();
                    let send = finished.borrow_mut().take().unwrap();
                    let (resource, immediate, roots) = (
                        resource.clone(),
                        first_immediate.clone(),
                        first_roots.clone(),
                    );
                    async move {
                        assert_eq!(ctx.principal(), &json!("host"));
                        assert_eq!(*resource.value()?, 7);
                        roots.borrow_mut().push(ctx.root_id());
                        immediate.replace(&ctx, json!(2), ReplaceOptions::default())?;
                        receive.await.unwrap();
                        send.send(()).unwrap();
                        Ok(json!(4))
                    }
                    .boxed_local()
                }),
            )?;
            let sibling = first.clone();
            let wait_finished = wait_finished.clone();
            let second = declaration.state(
                "second",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |ctx| {
                    let receive = wait_finished.borrow_mut().take().unwrap();
                    let (sibling, roots) = (sibling.clone(), roots.clone());
                    async move {
                        assert_eq!(ctx.principal(), &json!("host"));
                        roots.borrow_mut().push(ctx.root_id());
                        assert_eq!(sibling.snapshot().unwrap_err().code, ErrorCode::NotReady);
                        receive.await.unwrap();
                        assert_eq!(sibling.snapshot()?.value(), &json!(4));
                        Ok(json!(9))
                    }
                    .boxed_local()
                }),
            )?;
            let sum = declaration.derive(
                vec![first.readable(), second.readable()],
                json!({"type":"integer"}),
                |values| {
                    Ok(json!(
                        values[0].value().as_i64().unwrap() + values[1].value().as_i64().unwrap()
                    ))
                },
            )?;
            Ok(Implementation::new()
                .state("immediate", immediate.readable())
                .state("first", first.readable())
                .state("second", second.readable())
                .state("sum", sum))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    let immediate = reference.readable("immediate").unwrap();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let observed = seen.clone();
    let _subscription = immediate
        .subscribe(move |snapshot| {
            observed.borrow_mut().push(snapshot.value().clone());
            Ok(())
        })
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Loading);
    assert!(immediate.snapshot().is_err());
    assert!(seen.borrow().is_empty());
    assert_eq!(roots.borrow().len(), 2);
    assert_ne!(roots.borrow()[0], roots.borrow()[1]);
    start.send(()).unwrap();
    driver.run_until(reference.ready()).unwrap();
    assert_eq!(
        reference
            .readable("sum")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(13)
    );
    assert_eq!(&*seen.borrow(), &[json!(2)]);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn cancellation_does_not_close_resources_before_initializer_physically_settles() {
    let (host, driver) = host();
    let contract = contract(
        "late-initializer",
        1,
        &[("value", json!({"type":"integer"}))],
        &[],
    );
    let (send, receive) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(receive)));
    let settled = Rc::new(Counter::new(false));
    let closed = Rc::new(Counter::new(0));
    let cancelled = Rc::new(RefCell::new(None));
    let (completed, closes, token) = (settled.clone(), closed.clone(), cancelled.clone());
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let (settled, closed) = (completed.clone(), closes.clone());
            let resource = declaration.resource(
                "protected",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        assert!(settled.get(), "cleanup cannot race a running initializer");
                        closed.set(closed.get() + 1);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            let (gate, settled, token) = (gate.clone(), completed.clone(), token.clone());
            let cell = declaration.state(
                "value",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |ctx| {
                    *token.borrow_mut() = Some(ctx.cancellation().clone());
                    let receive = gate.borrow_mut().take().unwrap();
                    let (resource, settled) = (resource.clone(), settled.clone());
                    async move {
                        receive.await.unwrap();
                        resource.value()?;
                        settled.set(true);
                        Ok(json!(12))
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new().state("value", cell.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    reference.release();
    driver.run_until_stalled();
    assert!(cancelled.borrow().as_ref().unwrap().is_cancelled());
    assert_eq!(closed.get(), 0);
    driver.run_until(async {
        let mut closing = Box::pin(host.close());
        assert!(futures::poll!(closing.as_mut()).is_pending());
        drop(closing);
    });
    send.send(()).unwrap();
    driver.run_until(host.close()).unwrap();
    assert_eq!(closed.get(), 1);
    assert!(settled.get());
}

#[test]
fn factory_reads_initialized_cells_but_cannot_mutate_or_subscribe() {
    let (host, driver) = host();
    let contract = contract(
        "factory-state",
        1,
        &[("value", json!({"type":"integer"}))],
        &[],
    );
    let context = Rc::new(RefCell::new(None::<Context>));
    let active_context = context.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let cell = declaration.state(
                "value",
                StateDeclaration::new(json!({"type":"integer"}), json!(5)),
            )?;
            assert_eq!(cell.snapshot()?.value(), &json!(5));
            let ctx = active_context.borrow().as_ref().unwrap().clone();
            let called = Counter::new(false);
            let error = cell
                .change(&ctx, |_| {
                    called.set(true);
                    Ok(())
                })
                .unwrap_err();
            assert_eq!(error.reason, "read_only");
            assert!(!called.get());
            assert_eq!(
                cell.readable().subscribe(|_| Ok(())).err().unwrap().reason,
                "read_only"
            );
            Ok(Implementation::new().state("value", cell.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    driver
        .run_until(host.root(move |ctx| {
            *context.borrow_mut() = Some(ctx.clone());
            async move {
                let reference = ctx.get(contract, json!({}), GetOptions::default()).await?;
                assert_eq!(reference.readable("value")?.snapshot()?.value(), &json!(5));
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn external_forwarding_waits_without_declared_get_and_failed_projection_preserves_source() {
    let (host, driver) = host();
    let source_contract = contract(
        "external-source",
        1,
        &[("n", json!({"type":"integer"}))],
        &["set"],
    );
    let forward_contract = contract(
        "external-forward",
        1,
        &[
            ("mirror", json!({"type":"integer","minimum":0})),
            ("double", json!({"type":"integer"})),
        ],
        &[],
    );
    let (send, receive) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(receive)));
    host.serve(
        source_contract.clone(),
        Rc::new(move |_, _, declaration| {
            let gate = gate.clone();
            let cell = declaration.state(
                "n",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |_| {
                    let receive = gate.borrow_mut().take().unwrap();
                    async move {
                        receive.await.unwrap();
                        Ok(json!(3))
                    }
                    .boxed_local()
                }),
            )?;
            let changed = cell.clone();
            Ok(Implementation::new()
                .state("n", cell.readable())
                .json_method("set", move |value, ctx| {
                    let changed = changed.clone();
                    async move {
                        changed.replace(&ctx, value.unwrap(), ReplaceOptions::default())?;
                        Ok(None)
                    }
                    .boxed_local()
                }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let external = host
        .hold(source_contract, json!({}), GetOptions::default())
        .unwrap();
    let outside_factory = external.clone();
    host.serve(
        forward_contract.clone(),
        Rc::new(move |_, _, declaration| {
            let source = outside_factory.readable("n")?;
            let double =
                declaration.derive(vec![source.clone()], json!({"type":"integer"}), |values| {
                    Ok(json!(values[0].value().as_i64().unwrap() * 2))
                })?;
            Ok(Implementation::new()
                .state("mirror", source)
                .state("double", double))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let forward = host
        .hold(forward_contract, json!({}), GetOptions::default())
        .unwrap();
    let mirror = forward.readable("mirror").unwrap();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let observed = seen.clone();
    let _subscription = mirror
        .subscribe(move |snapshot| {
            observed.borrow_mut().push(snapshot.value().clone());
            Ok(())
        })
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(forward.status(), RefStatus::Loading);
    assert!(seen.borrow().is_empty());
    send.send(()).unwrap();
    driver.run_until(forward.ready()).unwrap();
    assert_eq!(&*seen.borrow(), &[json!(3)]);
    driver
        .run_until(external.call_json("set", Some(json!(-1)), None))
        .unwrap();
    assert_eq!(
        external.readable("n").unwrap().snapshot().unwrap().value(),
        &json!(-1)
    );
    assert_eq!(mirror.snapshot().unwrap().value(), &json!(3));
    assert_eq!(
        forward
            .readable("double")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(6)
    );
    assert!(!host.errors().is_empty());
    driver.run_until(host.close()).unwrap();
}

#[test]
fn adapter_external_state_delays_complete_binding_and_loading_subscriber_gets_snapshot() {
    let (host, driver) = host();
    let source_contract = contract(
        "adapter-source",
        1,
        &[("n", json!({"type":"integer"}))],
        &[],
    );
    let current = contract(
        "adapted-state",
        2,
        &[("current", json!({"type":"integer"}))],
        &[],
    );
    let legacy = contract(
        "adapted-state",
        1,
        &[("legacy", json!({"type":"integer"}))],
        &[],
    );
    let (send, receive) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(receive)));
    host.serve(
        source_contract.clone(),
        Rc::new(move |_, _, declaration| {
            let gate = gate.clone();
            let cell = declaration.state(
                "n",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |_| {
                    let receive = gate.borrow_mut().take().unwrap();
                    async move {
                        receive.await.unwrap();
                        Ok(json!(7))
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new().state("n", cell.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source = host
        .hold(source_contract, json!({}), GetOptions::default())
        .unwrap();
    host.serve(
        current.clone(),
        Rc::new(|_, _, declaration| {
            let cell = declaration.state(
                "current",
                StateDeclaration::new(json!({"type":"integer"}), json!(10)),
            )?;
            Ok(Implementation::new().state("current", cell.readable()))
        }),
        ServeOptions::default().with_adapter(
            1,
            Rc::new(move |_| Ok(Implementation::new().state("legacy", source.readable("n")?))),
        ),
    )
    .unwrap();
    let provider = host
        .hold(current, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until(provider.ready()).unwrap();
    assert_eq!(provider.status(), RefStatus::Live);
    let consumer = host.hold(legacy, json!({}), GetOptions::default()).unwrap();
    assert_eq!(consumer.status(), RefStatus::Loading);
    let state = consumer.readable("legacy").unwrap();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let observed = seen.clone();
    let _subscription = state
        .subscribe_events(move |event| {
            assert!(matches!(event, StateEvent::Snapshot(_)));
            observed.borrow_mut().push(event.snapshot().value().clone());
            Ok(())
        })
        .unwrap();
    let status_values = Rc::new(RefCell::new(Vec::new()));
    let status_seen = status_values.clone();
    let ready_state = state.clone();
    let _stop = consumer.on_status(move |status, _| {
        if status == RefStatus::Live {
            status_seen
                .borrow_mut()
                .push(ready_state.snapshot().unwrap().value().clone());
        }
    });
    driver.run_until_stalled();
    assert!(seen.borrow().is_empty());
    send.send(()).unwrap();
    driver.run_until(consumer.ready()).unwrap();
    assert_eq!(&*seen.borrow(), &[json!(7)]);
    assert_eq!(&*status_values.borrow(), &[json!(7)]);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn dropping_pending_adapter_binding_does_not_retain_its_object() {
    let (host, driver) = host();
    let source_contract = contract("weak-source", 1, &[("n", json!({"type":"integer"}))], &[]);
    let current = contract("weak-adapter", 2, &[], &[]);
    let legacy = contract(
        "weak-adapter",
        1,
        &[("legacy", json!({"type":"integer"}))],
        &[],
    );
    let (send, receive) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(receive)));
    host.serve(
        source_contract.clone(),
        Rc::new(move |_, _, declaration| {
            let gate = gate.clone();
            let cell = declaration.state(
                "n",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |_| {
                    let receive = gate.borrow_mut().take().unwrap();
                    async move {
                        receive.await.unwrap();
                        Ok(json!(7))
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new().state("n", cell.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source = host
        .hold(source_contract, json!({}), GetOptions::default())
        .unwrap();
    let closes = Rc::new(Counter::new(0));
    let closed = closes.clone();
    host.serve(
        current.clone(),
        Rc::new(move |_, _, declaration| {
            let closed = closed.clone();
            declaration.resource(
                "owned",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        closed.set(closed.get() + 1);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            Ok(Implementation::new())
        }),
        ServeOptions::default().with_adapter(
            1,
            Rc::new(move |_| Ok(Implementation::new().state("legacy", source.readable("n")?))),
        ),
    )
    .unwrap();
    let provider = driver
        .run_until(host.get(current, json!({}), GetOptions::default()))
        .unwrap();
    let consumer = host.hold(legacy, json!({}), GetOptions::default()).unwrap();
    let weak_readable = consumer.readable("legacy").unwrap();
    driver.run_until_stalled();
    assert_eq!(consumer.status(), RefStatus::Loading);
    drop(consumer);
    provider.release();
    driver.run_until_stalled();
    assert_eq!(closes.get(), 1);
    assert!(weak_readable.snapshot().is_err());
    send.send(()).unwrap();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn failed_initializer_waits_for_running_siblings_before_cleanup() {
    let (host, driver) = host();
    let contract = contract(
        "failed-initializer",
        1,
        &[("first", json!({})), ("second", json!({}))],
        &[],
    );
    let (send, receive) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(receive)));
    let settled = Rc::new(Counter::new(false));
    let closed = Rc::new(Counter::new(false));
    let (settled_factory, closed_factory) = (settled.clone(), closed.clone());
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let (settled_close, closed) = (settled_factory.clone(), closed_factory.clone());
            let resource = declaration.resource(
                "protected",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        assert!(settled_close.get());
                        closed.set(true);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            let (started, wait_started) = oneshot::channel();
            let wait_started = Rc::new(RefCell::new(Some(wait_started)));
            let started = Rc::new(RefCell::new(Some(started)));
            let first = declaration.state(
                "first",
                StateDeclaration::with_initializer(json!({}), move |_| {
                    let wait_started = wait_started.borrow_mut().take().unwrap();
                    async move {
                        wait_started.await.unwrap();
                        Err(RuntimeError::invalid(
                            "initializer_witness",
                            "first initializer refused",
                        ))
                    }
                    .boxed_local()
                }),
            )?;
            let (gate, settled) = (gate.clone(), settled_factory.clone());
            let second = declaration.state(
                "second",
                StateDeclaration::with_initializer(json!({}), move |_| {
                    let receive = gate.borrow_mut().take().unwrap();
                    let started = started.borrow_mut().take().unwrap();
                    let (resource, settled) = (resource.clone(), settled.clone());
                    async move {
                        started.send(()).unwrap();
                        receive.await.unwrap();
                        resource.value()?;
                        settled.set(true);
                        Ok(json!(2))
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new()
                .state("first", first.readable())
                .state("second", second.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Loading);
    assert!(!closed.get());
    send.send(()).unwrap();
    let error = driver.run_until(reference.ready()).unwrap_err();
    assert_eq!(error.reason, "initializer_witness");
    driver.run_until_stalled();
    assert!(closed.get());
    driver.run_until(host.close()).unwrap();
}

#[test]
fn initializer_self_acquisition_is_refused_without_waiting_for_timeout() {
    let (host, driver) = host();
    let contract = contract("initializer-cycle", 1, &[("value", json!({}))], &[]);
    let requested = contract.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let requested = requested.clone();
            let value = declaration.state(
                "value",
                StateDeclaration::with_initializer(json!({}), move |ctx| {
                    let requested = requested.clone();
                    async move {
                        ctx.get(requested, json!({}), GetOptions::default()).await?;
                        Ok(json!(0))
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new().state("value", value.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Gone);
    assert_eq!(reference.error().unwrap().reason, "cycle");
    driver.run_until(host.close()).unwrap();
}

#[test]
fn initializer_wait_edges_detect_a_cycle_between_independently_acquired_objects() {
    let (host, driver) = host();
    let first = contract("initializer-first", 1, &[("value", json!({}))], &[]);
    let second = contract("initializer-second", 1, &[("value", json!({}))], &[]);
    for (provided, requested) in [
        (first.clone(), second),
        (
            contract("initializer-second", 1, &[("value", json!({}))], &[]),
            first.clone(),
        ),
    ] {
        host.serve(
            provided,
            Rc::new(move |_, _, declaration| {
                let requested = requested.clone();
                let value = declaration.state(
                    "value",
                    StateDeclaration::with_initializer(json!({}), move |ctx| {
                        let requested = requested.clone();
                        async move {
                            ctx.get(requested, json!({}), GetOptions::default()).await?;
                            Ok(json!(0))
                        }
                        .boxed_local()
                    }),
                )?;
                Ok(Implementation::new().state("value", value.readable()))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    }
    let reference = host.hold(first, json!({}), GetOptions::default()).unwrap();
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Gone);
    assert_eq!(reference.error().unwrap().reason, "cycle");
    driver.run_until(host.close()).unwrap();
}

#[test]
fn invalid_adapter_state_never_announces_live_and_does_not_damage_provider() {
    let (host, driver) = host();
    let source_contract = contract(
        "invalid-adapter-source",
        1,
        &[("value", json!({"type":"string"}))],
        &[],
    );
    host.serve(
        source_contract.clone(),
        Rc::new(|_, _, declaration| {
            let value = declaration.state(
                "value",
                StateDeclaration::new(json!({"type":"string"}), json!("wrong type")),
            )?;
            Ok(Implementation::new().state("value", value.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source = driver
        .run_until(host.get(source_contract, json!({}), GetOptions::default()))
        .unwrap();
    let current = contract("invalid-adapter", 2, &[], &[]);
    let legacy = contract(
        "invalid-adapter",
        1,
        &[("value", json!({"type":"integer"}))],
        &[],
    );
    host.serve(
        current.clone(),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions::default().with_adapter(
            1,
            Rc::new(move |_| Ok(Implementation::new().state("value", source.readable("value")?))),
        ),
    )
    .unwrap();
    let provider = driver
        .run_until(host.get(current, json!({}), GetOptions::default()))
        .unwrap();
    let consumer = host.hold(legacy, json!({}), GetOptions::default()).unwrap();
    let observed = Rc::new(RefCell::new(Vec::new()));
    let statuses = observed.clone();
    let _stop = consumer.on_status(move |status, _| statuses.borrow_mut().push(status));
    assert_eq!(
        driver.run_until(consumer.ready()).unwrap_err().reason,
        "state"
    );
    assert_eq!(consumer.status(), RefStatus::Gone);
    assert!(!observed.borrow().contains(&RefStatus::Live));
    assert_eq!(provider.status(), RefStatus::Live);
    driver.run_until(provider.ready()).unwrap();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn failed_binding_hydration_does_not_wait_for_other_pending_sources() {
    let (host, driver) = host();
    let mut sources = Vec::new();
    let mut completions = Vec::new();
    for name in ["hydration-first", "hydration-second"] {
        let contract = contract(name, 1, &[("value", json!({"type":"integer"}))], &[]);
        let (send, receive) = oneshot::channel();
        let gate = Rc::new(RefCell::new(Some(receive)));
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let gate = gate.clone();
                let cell = declaration.state(
                    "value",
                    StateDeclaration::with_initializer(json!({"type":"integer"}), move |_| {
                        let receive = gate.borrow_mut().take().unwrap();
                        async move {
                            receive.await.unwrap();
                            Ok(json!(1))
                        }
                        .boxed_local()
                    }),
                )?;
                Ok(Implementation::new().state("value", cell.readable()))
            }),
            ServeOptions::default(),
        )
        .unwrap();
        sources.push(
            host.hold(contract, json!({}), GetOptions::default())
                .unwrap(),
        );
        completions.push(send);
    }
    let first = sources[0].clone();
    let current = contract("hydration-adapter", 2, &[], &[]);
    let legacy = contract(
        "hydration-adapter",
        1,
        &[
            ("first", json!({"type":"integer"})),
            ("second", json!({"type":"integer"})),
        ],
        &[],
    );
    host.serve(
        current,
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions::default().with_adapter(
            1,
            Rc::new(move |_| {
                Ok(Implementation::new()
                    .state("first", sources[0].readable("value")?)
                    .state("second", sources[1].readable("value")?))
            }),
        ),
    )
    .unwrap();
    let consumer = host.hold(legacy, json!({}), GetOptions::default()).unwrap();
    driver.run_until_stalled();
    assert_eq!(consumer.status(), RefStatus::Loading);
    first.release();
    driver.run_until_stalled();
    assert_eq!(consumer.status(), RefStatus::Gone);
    assert_eq!(consumer.error().unwrap().code, ErrorCode::Released);
    for complete in completions {
        complete.send(()).unwrap();
    }
    driver.run_until(host.close()).unwrap();
}

#[test]
fn first_initializer_failure_cancels_siblings_without_releasing_the_claim() {
    let (host, driver) = host();
    let contract = contract("initializer-cancel", 1, &[], &[]);
    let cancelled = Rc::new(Counter::new(false));
    let closed = Rc::new(Counter::new(false));
    let (observed, closed_resource) = (cancelled.clone(), closed.clone());
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let (observed_close, closed) = (observed.clone(), closed_resource.clone());
            let resource = declaration.resource(
                "protected",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        assert!(
                            observed_close.get(),
                            "the sibling must physically settle before cleanup"
                        );
                        closed.set(true);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            let (started, wait_started) = oneshot::channel();
            let wait_started = Rc::new(RefCell::new(Some(wait_started)));
            let started = Rc::new(RefCell::new(Some(started)));
            declaration.state(
                "failing",
                StateDeclaration::with_initializer(json!({}), move |_| {
                    let wait = wait_started.borrow_mut().take().unwrap();
                    async move {
                        wait.await.unwrap();
                        Err(RuntimeError::invalid(
                            "first_failure",
                            "initializer refused",
                        ))
                    }
                    .boxed_local()
                }),
            )?;
            let observed = observed.clone();
            declaration.state(
                "cancellation-aware",
                StateDeclaration::with_initializer(json!({}), move |ctx| {
                    let start = started.borrow_mut().take().unwrap();
                    let (observed, resource) = (observed.clone(), resource.clone());
                    async move {
                        start.send(()).unwrap();
                        let error = ctx.cancellation().cancelled().await;
                        resource.value()?;
                        observed.set(true);
                        Err(error)
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Gone);
    assert_eq!(reference.error().unwrap().reason, "first_failure");
    assert!(cancelled.get());
    assert!(closed.get());
    driver.run_until(host.close()).unwrap();
}

#[test]
fn live_claim_does_not_retain_its_initial_state_snapshot_after_a_write() {
    let (host, driver) = host();
    let contract = contract(
        "initial-snapshot-retention",
        1,
        &[("value", json!({"type":"integer"}))],
        &[],
    );
    let retained = Rc::new(RefCell::new(None::<Cell>));
    let saved = retained.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let cell = declaration.state(
                "value",
                StateDeclaration::new(json!({"type":"integer"}), json!(1)),
            )?;
            *saved.borrow_mut() = Some(cell.clone());
            Ok(Implementation::new().state("value", cell.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    let readable = reference.readable("value").unwrap();
    let initial = Rc::downgrade(&readable.snapshot().unwrap().shared_value());
    driver
        .run_until(host.root(move |ctx| {
            async move {
                retained.borrow().as_ref().unwrap().replace(
                    &ctx,
                    json!(2),
                    ReplaceOptions::default(),
                )?;
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(readable.snapshot().unwrap().value(), &json!(2));
    assert!(
        initial.upgrade().is_none(),
        "a live claim must release its staging snapshot"
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn ended_external_reference_preserves_last_forwarded_projection_without_an_event() {
    let (host, driver) = host();
    let source_contract = contract("ended-source", 1, &[("n", json!({"type":"integer"}))], &[]);
    host.serve(
        source_contract.clone(),
        Rc::new(|_, _, declaration| {
            let cell = declaration.state(
                "n",
                StateDeclaration::new(json!({"type":"integer"}), json!(7)),
            )?;
            Ok(Implementation::new().state("n", cell.readable()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source = driver
        .run_until(host.get(source_contract, json!({}), GetOptions::default()))
        .unwrap();
    let direct = source.readable("n").unwrap();
    let forward_contract = contract("ended-forward", 1, &[("n", json!({"type":"integer"}))], &[]);
    let forwarded = source.clone();
    host.serve(
        forward_contract.clone(),
        Rc::new(move |_, _, _| Ok(Implementation::new().state("n", forwarded.readable("n")?))),
        ServeOptions::default(),
    )
    .unwrap();
    let forward = driver
        .run_until(host.get(forward_contract, json!({}), GetOptions::default()))
        .unwrap();
    let projection = forward.readable("n").unwrap();
    let original = projection.snapshot().unwrap();
    let events = Rc::new(Counter::new(0));
    let observed = events.clone();
    let _subscription = projection
        .subscribe_events(move |_| {
            observed.set(observed.get() + 1);
            Ok(())
        })
        .unwrap();
    source.release();
    driver.run_until_stalled();
    assert_eq!(direct.snapshot().unwrap_err().code, ErrorCode::Released);
    assert_eq!(forward.status(), RefStatus::Live);
    assert_eq!(projection.snapshot().unwrap(), original);
    assert_eq!(
        events.get(),
        1,
        "termination must not invent a reset or value event"
    );
    assert!(host.errors().is_empty());
    driver.run_until(host.close()).unwrap();
}

#[test]
fn adapter_hydration_uses_current_values_after_another_source_waits() {
    let (host, driver) = host();
    let source_contract = contract("adapter-race-source", 1, &[], &[]);
    let cells = Rc::new(RefCell::new(None::<(Cell, Cell)>));
    let saved = cells.clone();
    let (start_second, wait_second) = oneshot::channel();
    let wait_second = Rc::new(RefCell::new(Some(wait_second)));
    let (finish_source, wait_source) = oneshot::channel();
    let wait_source = Rc::new(RefCell::new(Some(wait_source)));
    host.serve(
        source_contract.clone(),
        Rc::new(move |_, _, declaration| {
            let first = declaration.state(
                "first",
                StateDeclaration::new(json!({"type":"integer"}), json!(1)),
            )?;
            let wait = wait_second.clone();
            let second = declaration.state(
                "second",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |_| {
                    let wait = wait.borrow_mut().take().unwrap();
                    async move {
                        wait.await.unwrap();
                        Ok(json!(10))
                    }
                    .boxed_local()
                }),
            )?;
            let wait = wait_source.clone();
            declaration.state(
                "activation-gate",
                StateDeclaration::with_initializer(json!({}), move |_| {
                    let wait = wait.borrow_mut().take().unwrap();
                    async move {
                        wait.await.unwrap();
                        Ok(json!(null))
                    }
                    .boxed_local()
                }),
            )?;
            *saved.borrow_mut() = Some((first, second));
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source = host
        .hold(source_contract, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    let current = contract("adapter-race", 2, &[], &[]);
    let legacy = contract(
        "adapter-race",
        1,
        &[
            ("first", json!({"type":"integer"})),
            ("second", json!({"type":"integer"})),
        ],
        &[],
    );
    let forwarded = cells.clone();
    host.serve(
        current,
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions::default().with_adapter(
            1,
            Rc::new(move |_| {
                let cells = forwarded.borrow();
                let (first, second) = cells.as_ref().unwrap();
                Ok(Implementation::new()
                    .state("first", first.readable())
                    .state("second", second.readable()))
            }),
        ),
    )
    .unwrap();
    let consumer = host.hold(legacy, json!({}), GetOptions::default()).unwrap();
    driver.run_until_stalled();
    assert_eq!(consumer.status(), RefStatus::Loading);
    let first = cells.borrow().as_ref().unwrap().0.clone();
    driver
        .run_until(host.root(move |ctx| {
            async move {
                first.replace(&ctx, json!(2), ReplaceOptions::default())?;
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    start_second.send(()).unwrap();
    driver.run_until(consumer.ready()).unwrap();
    assert_eq!(
        source.status(),
        RefStatus::Loading,
        "the source's final publication is still gated"
    );
    assert_eq!(
        consumer
            .readable("first")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(2)
    );
    assert_eq!(
        consumer
            .readable("second")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(10)
    );
    finish_source.send(()).unwrap();
    driver.run_until(host.close()).unwrap();
}
