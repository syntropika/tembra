use futures::{FutureExt, StreamExt, channel::oneshot};
use serde_json::{Value, json};
use std::{
    cell::{Cell as Counter, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{
    contracts::{Contract, Member, Placement, ValueSchema},
    rpc::*,
    *,
};

fn method(args: Option<Value>, result: Option<Value>) -> Member {
    Member::Method {
        args: args.map(ValueSchema::Portable),
        result: result.map(ValueSchema::Portable),
    }
}
fn local(name: &str, version: u64, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(Contract::local(name, version, json!({}), members).unwrap())
}
fn remote(
    name: &str,
    version: u64,
    target: &str,
    members: BTreeMap<String, Member>,
) -> Rc<Contract> {
    Rc::new(
        Contract::remote(
            name,
            version,
            json!({}),
            Placement::Named {
                name: target.into(),
            },
            BTreeMap::from([("default".into(), members)]),
        )
        .unwrap(),
    )
}
fn host(name: &str, driver: &Rc<ManualDriver>) -> Host {
    let host = Host::new(
        HostOptions::new(name, json!(format!("{name}-principal"))),
        driver.clone(),
    );
    let incarnation = if name == "server" {
        "11111111111111111111111111111111"
    } else {
        "22222222222222222222222222222222"
    };
    host.set_rpc_identity(RootOrigin::new(name, incarnation).unwrap())
        .unwrap();
    host
}
fn connect(
    client: &Host,
    server: &Host,
    driver: &Rc<ManualDriver>,
) -> (Connection, Connection, MemoryControl) {
    let (left, right, control) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let outbound = client
        .connect_client(left, server.name(), ConnectionOptions::default())
        .unwrap();
    let inbound = server
        .accept_client(right, json!("accepted-user"), ConnectionOptions::default())
        .unwrap();
    client.set_route(server.name(), outbound.clone()).unwrap();
    (outbound, inbound, control)
}

#[test]
fn method_only_local_adapter_readiness_precedes_state_and_interest_is_removable() {
    let driver = ManualDriver::new();
    let host = host("server", &driver);
    let source_contract = local("method-only-source", 1, BTreeMap::new());
    let saved = Rc::new(RefCell::new(None::<Cell>));
    let factory_saved = saved.clone();
    let (complete, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    host.serve(
        source_contract.clone(),
        Rc::new(move |_, _, declaration| {
            let receive = receive.clone();
            let state = declaration.state(
                "pending",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |_| {
                    let receive = receive.borrow_mut().take().unwrap();
                    async move {
                        receive.await.unwrap();
                        Ok(json!(5))
                    }
                    .boxed_local()
                }),
            )?;
            *factory_saved.borrow_mut() = Some(state);
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source = host
        .hold(source_contract, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    let current = local(
        "method-only-adapter",
        2,
        BTreeMap::from([("read".into(), method(None, Some(json!({"type":"integer"}))))]),
    );
    let legacy = local(
        "method-only-adapter",
        1,
        BTreeMap::from([
            ("read".into(), method(None, Some(json!({"type":"integer"})))),
            (
                "pending".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
        ]),
    );
    host.serve(
        current,
        Rc::new(|_, _, _| {
            Ok(Implementation::new()
                .json_method("read", |_, _| async { Ok(Some(json!(9))) }.boxed_local()))
        }),
        ServeOptions::default().with_adapter(
            1,
            Rc::new(move |implementation| {
                Ok(Implementation::new()
                    .json_method("read", {
                        let call = implementation.portable_method("read")?;
                        move |args, ctx| call(args, ctx)
                    })
                    .state("pending", saved.borrow().as_ref().unwrap().readable()))
            }),
        ),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(
            legacy,
            json!({}),
            GetOptions {
                state: false,
                ..Default::default()
            },
        ))
        .unwrap();
    assert_eq!(reference.status(), RefStatus::Live);
    assert_eq!(
        driver
            .run_until(reference.call_json("read", None, None))
            .unwrap(),
        Some(json!(9))
    );
    let readable = reference.readable("pending").unwrap();
    assert_eq!(readable.snapshot().unwrap_err().code, ErrorCode::NotReady);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let first = seen.clone();
    let subscription = readable
        .subscribe(move |snapshot| {
            first.borrow_mut().push(snapshot.value().clone());
            Ok(())
        })
        .unwrap();
    subscription.unsubscribe();
    complete.send(()).unwrap();
    driver.run_until(source.ready()).unwrap();
    assert!(seen.borrow().is_empty());
    assert!(
        readable.snapshot().is_err(),
        "a removed interest must not hydrate its pending proxy later"
    );
    let next = seen.clone();
    let _subscription = readable
        .subscribe(move |snapshot| {
            next.borrow_mut().push(snapshot.value().clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(readable.snapshot().unwrap().value(), &json!(5));
    assert_eq!(&*seen.borrow(), &[json!(5)]);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn memory_rpc_preserves_presence_state_and_independent_claims() {
    let driver = ManualDriver::new();
    let server = host("server", &driver);
    let client = host("client", &driver);
    let contract = remote(
        "rpc.counter",
        1,
        "server",
        BTreeMap::from([
            (
                "count".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
            (
                "double".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
            (
                "add".into(),
                method(
                    Some(json!({"type":"integer"})),
                    Some(json!({"type":"integer"})),
                ),
            ),
            (
                "nullable".into(),
                method(
                    Some(json!({"type":["integer","null"]})),
                    Some(json!({"type":["integer","null"]})),
                ),
            ),
            ("void".into(), method(None, None)),
            (
                "principal".into(),
                method(None, Some(json!({"type":"string"}))),
            ),
        ]),
    );
    let factories = Rc::new(Counter::new(0));
    let created = factories.clone();
    server
        .serve(
            contract.clone(),
            Rc::new(move |_, principal, declaration| {
                created.set(created.get() + 1);
                let count = declaration.state(
                    "count",
                    StateDeclaration::new(json!({"type":"integer"}), json!(0)),
                )?;
                let double = declaration.derive(
                    vec![count.readable()],
                    json!({"type":"integer"}),
                    |values| Ok(json!(values[0].value().as_i64().unwrap() * 2)),
                )?;
                Ok(Implementation::new()
                    .variant("default")
                    .state("count", count.readable())
                    .state("double", double)
                    .json_method("add", move |args, ctx| {
                        let count = count.clone();
                        async move {
                            let next = count.snapshot()?.value().as_i64().unwrap()
                                + args.unwrap().as_i64().unwrap();
                            count.replace(&ctx, json!(next), ReplaceOptions { diff: true })?;
                            Ok(Some(json!(next)))
                        }
                        .boxed_local()
                    })
                    .json_method("nullable", |args, _| async move { Ok(args) }.boxed_local())
                    .json_method("void", |args, _| {
                        async move {
                            assert!(args.is_none());
                            Ok(None)
                        }
                        .boxed_local()
                    })
                    .json_method("principal", move |_, _| {
                        let principal = principal.clone();
                        async move { Ok(Some(principal)) }.boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let (_outbound, _inbound, _control) = connect(&client, &server, &driver);
    let first = driver
        .run_until(client.get(contract.clone(), json!({}), GetOptions::default()))
        .unwrap();
    let second = driver
        .run_until(client.get(
            contract,
            json!({}),
            GetOptions {
                principal: Some(json!("forged-local")),
                ..Default::default()
            },
        ))
        .unwrap();
    assert_eq!(factories.get(), 1);
    assert_eq!(first.object_id(), second.object_id());
    assert_eq!(
        driver
            .run_until(first.call_json("principal", None, None))
            .unwrap(),
        Some(json!("accepted-user"))
    );
    assert_eq!(
        driver
            .run_until(first.call_json("nullable", Some(Value::Null), None))
            .unwrap(),
        Some(Value::Null)
    );
    assert_eq!(
        driver
            .run_until(first.call_json("void", None, None))
            .unwrap(),
        None
    );
    assert_eq!(
        driver
            .run_until(first.call_json("nullable", None, None))
            .unwrap_err()
            .reason,
        "args"
    );
    let double = first.readable("double").unwrap();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let observed = seen.clone();
    let _subscription = first
        .readable("count")
        .unwrap()
        .subscribe(move |snapshot| {
            observed
                .borrow_mut()
                .push((snapshot.value().clone(), double.snapshot()?.value().clone()));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver
            .run_until(first.call_json("add", Some(json!(3)), None))
            .unwrap(),
        Some(json!(3))
    );
    driver.run_until_stalled();
    assert_eq!(
        second
            .readable("count")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(3)
    );
    assert_eq!(seen.borrow()[0], (json!(0), json!(0)));
    assert_eq!(seen.borrow().last().unwrap().0, json!(3));
    assert_eq!(
        second
            .readable("double")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(6)
    );
    first.release();
    assert_eq!(second.status(), RefStatus::Live);
    assert_eq!(
        driver
            .run_until(second.call_json("add", Some(json!(2)), None))
            .unwrap(),
        Some(json!(5))
    );
    driver.run_until(client.close()).unwrap();
    driver.run_until(server.close()).unwrap();
}

#[test]
fn reconnect_reuses_readable_and_resets_a_lower_provider_revision() {
    let driver = ManualDriver::new();
    let server = host("server", &driver);
    let client = host("client", &driver);
    let contract = remote(
        "rpc.reconnect",
        1,
        "server",
        BTreeMap::from([
            (
                "n".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
            ("set".into(), method(Some(json!({"type":"integer"})), None)),
        ]),
    );
    server
        .serve(
            contract.clone(),
            Rc::new(|_, _, declaration| {
                let cell = declaration.state(
                    "n",
                    StateDeclaration::new(json!({"type":"integer"}), json!(0)),
                )?;
                Ok(Implementation::new()
                    .variant("default")
                    .state("n", cell.readable())
                    .json_method("set", move |args, ctx| {
                        let cell = cell.clone();
                        async move {
                            cell.replace(&ctx, args.unwrap(), ReplaceOptions::default())?;
                            Ok(None)
                        }
                        .boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let (outbound, _inbound, _control) = connect(&client, &server, &driver);
    let reference = driver
        .run_until(client.get(contract.clone(), json!({}), GetOptions::default()))
        .unwrap();
    let readable = reference.readable("n").unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    let _subscription = readable
        .subscribe_events(move |event| {
            observed.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    driver
        .run_until(reference.call_json("set", Some(json!(7)), None))
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(readable.snapshot().unwrap().value(), &json!(7));
    let old_revision = readable.snapshot().unwrap().revision();
    outbound.close(CloseReason::closed());
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Loading);
    assert_eq!(readable.snapshot().unwrap().value(), &json!(7));
    let during_outage = client
        .hold(contract.clone(), json!({}), GetOptions::default())
        .unwrap();
    let outage_readable = during_outage.readable("n").unwrap();
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Loading);
    assert_eq!(during_outage.status(), RefStatus::Loading);
    assert_eq!(readable.snapshot().unwrap().value(), &json!(7));
    let (_new_outbound, _new_inbound, _new_control) = connect(&client, &server, &driver);
    driver.run_until(reference.ready()).unwrap();
    driver.run_until(during_outage.ready()).unwrap();
    assert_eq!(outage_readable.snapshot().unwrap().value(), &json!(0));
    assert_eq!(readable.snapshot().unwrap().value(), &json!(0));
    assert!(readable.snapshot().unwrap().revision() < old_revision);
    assert!(
        matches!(events.borrow().last(), Some(StateEvent::Reset(snapshot)) if snapshot.value() == &json!(0))
    );
    driver.run_until(client.close()).unwrap();
    driver.run_until(server.close()).unwrap();
}

#[test]
fn method_only_remote_demand_stops_after_last_subscription_and_resumes_with_reset() {
    let driver = ManualDriver::new();
    let server = host("server", &driver);
    let client = host("client", &driver);
    let contract = remote(
        "rpc.interest",
        1,
        "server",
        BTreeMap::from([
            (
                "n".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
            ("set".into(), method(Some(json!({"type":"integer"})), None)),
        ]),
    );
    server
        .serve(
            contract.clone(),
            Rc::new(|_, _, declaration| {
                let cell = declaration.state(
                    "n",
                    StateDeclaration::new(json!({"type":"integer"}), json!(0)),
                )?;
                Ok(Implementation::new()
                    .variant("default")
                    .state("n", cell.readable())
                    .json_method("set", move |args, ctx| {
                        let cell = cell.clone();
                        async move {
                            cell.replace(&ctx, args.unwrap(), ReplaceOptions::default())?;
                            Ok(None)
                        }
                        .boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let (_outbound, _inbound, _control) = connect(&client, &server, &driver);
    let reference = driver
        .run_until(client.get(
            contract,
            json!({}),
            GetOptions {
                state: false,
                ..Default::default()
            },
        ))
        .unwrap();
    let readable = reference.readable("n").unwrap();
    assert_eq!(readable.snapshot().unwrap_err().code, ErrorCode::NotReady);
    driver
        .run_until(reference.call_json("set", Some(json!(1)), None))
        .unwrap();
    driver.run_until_stalled();
    assert!(readable.snapshot().is_err(), "peek must not request state");
    let first = readable.subscribe(|_| Ok(())).unwrap();
    let second = readable.subscribe(|_| Ok(())).unwrap();
    driver.run_until(readable.await_hydrated()).unwrap();
    assert_eq!(readable.snapshot().unwrap().value(), &json!(1));
    first.unsubscribe();
    driver
        .run_until(reference.call_json("set", Some(json!(2)), None))
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(readable.snapshot().unwrap().value(), &json!(2));
    second.unsubscribe();
    driver.run_until_stalled();
    driver
        .run_until(reference.call_json("set", Some(json!(3)), None))
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(
        readable.snapshot().unwrap().value(),
        &json!(2),
        "method-only cached peek may be stale without demand"
    );
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    let _resumed = readable
        .subscribe_events(move |event| {
            observed.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(readable.snapshot().unwrap().value(), &json!(3));
    assert!(
        matches!(events.borrow().last(), Some(StateEvent::Reset(snapshot)) if snapshot.value() == &json!(3))
    );
    driver.run_until(client.close()).unwrap();
    driver.run_until(server.close()).unwrap();
}

#[test]
fn trusted_root_bounce_preserves_owner_scope_after_child_returns() {
    let driver = ManualDriver::new();
    let origin = host("server", &driver);
    let worker = host("worker", &driver);
    let check = remote(
        "rpc.root-check",
        1,
        "server",
        BTreeMap::from([(
            "check".into(),
            method(None, Some(json!({"type":"integer"}))),
        )]),
    );
    let hop = remote(
        "rpc.root-hop",
        1,
        "worker",
        BTreeMap::from([("hop".into(), method(None, Some(json!({"type":"integer"}))))]),
    );
    origin
        .serve(
            check.clone(),
            Rc::new(|_, _, _| {
                Ok(Implementation::new()
                    .variant("default")
                    .json_method("check", |_, ctx| {
                        async move { Ok(Some(json!(ctx.root_id()))) }.boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    worker
        .serve(
            hop.clone(),
            Rc::new(move |_, _, _| {
                let check = check.clone();
                Ok(Implementation::new()
                    .variant("default")
                    .json_method("hop", move |_, ctx| {
                        let check = check.clone();
                        async move {
                            let reference =
                                ctx.get(check, json!({}), GetOptions::default()).await?;
                            reference.call_json("check", None, Some(&ctx)).await
                        }
                        .boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let (left, right, _control) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let down = origin
        .accept_child(
            left,
            "worker",
            json!("worker"),
            ConnectionOptions::default(),
        )
        .unwrap();
    let up = worker
        .connect_parent(
            right,
            "server",
            json!("server"),
            ConnectionOptions::default(),
        )
        .unwrap();
    origin.set_route("worker", down).unwrap();
    worker.set_route("server", up).unwrap();
    driver
        .run_until(origin.root(move |ctx| {
            async move {
                let expected = ctx.root_id();
                let reference = ctx.get(hop, json!({}), GetOptions::default()).await?;
                for _ in 0..2 {
                    assert_eq!(
                        reference.call_json("hop", None, Some(&ctx)).await?,
                        Some(json!(expected))
                    );
                    ctx.check(ctx.host_id())?;
                }
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    driver.run_until_stalled();
    driver.run_until(origin.close()).unwrap();
    driver.run_until(worker.close()).unwrap();
}

#[test]
fn route_authorization_is_rechecked_after_suspended_resolution() {
    let driver = ManualDriver::new();
    let server = host("server", &driver);
    let client = host("client", &driver);
    let token = remote("rpc.authorized-route", 1, "server", BTreeMap::new());
    let effects = Rc::new(Counter::new(0));
    let created = effects.clone();
    server
        .serve(
            token.clone(),
            Rc::new(move |_, _, _| {
                created.set(created.get() + 1);
                Ok(Implementation::new().variant("default"))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let (left, right, _control) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let outgoing = client
        .connect_client(left, "server", ConnectionOptions::default())
        .unwrap();
    let _incoming = server
        .accept_client(right, json!("authenticated"), ConnectionOptions::default())
        .unwrap();
    let allowed = Rc::new(Counter::new(true));
    let authorizations = Rc::new(Counter::new(0));
    let (permission, calls) = (allowed.clone(), authorizations.clone());
    client
        .set_route_authorizer(Rc::new(move |request| {
            assert_eq!(request.target, "server");
            calls.set(calls.get() + 1);
            if permission.get() {
                Ok(())
            } else {
                Err(RuntimeError::new(
                    ErrorCode::Forbidden,
                    "denied",
                    "route revoked",
                ))
            }
        }))
        .unwrap();
    let (resolve, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    client
        .set_resolver(Rc::new(move |_| {
            let receive = receive.borrow_mut().take().unwrap();
            async move {
                receive
                    .await
                    .map_err(|_| RuntimeError::unavailable("route", "resolver ended"))
            }
            .boxed_local()
        }))
        .unwrap();
    let reference = client
        .hold(token, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(authorizations.get(), 1);
    allowed.set(false);
    assert!(resolve.send(outgoing).is_ok());
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Gone);
    assert_eq!(reference.error().unwrap().reason, "denied");
    assert_eq!(authorizations.get(), 2);
    assert_eq!(
        effects.get(),
        0,
        "late unauthorized resolution must not create a provider object"
    );
    driver.run_until(client.close()).unwrap();
    driver.run_until(server.close()).unwrap();
}

#[test]
fn releasing_a_pending_route_does_not_admit_its_late_resolver_result() {
    let driver = ManualDriver::new();
    let server = host("server", &driver);
    let client = host("client", &driver);
    let token = remote("rpc.abandoned-route", 1, "server", BTreeMap::new());
    let effects = Rc::new(Counter::new(0));
    let created = effects.clone();
    server
        .serve(
            token.clone(),
            Rc::new(move |_, _, _| {
                created.set(created.get() + 1);
                Ok(Implementation::new().variant("default"))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let (left, right, _control) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let outgoing = client
        .connect_client(left, "server", ConnectionOptions::default())
        .unwrap();
    let _incoming = server
        .accept_client(right, json!("authenticated"), ConnectionOptions::default())
        .unwrap();
    let (resolve, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    client
        .set_resolver(Rc::new(move |_| {
            let receive = receive.borrow_mut().take().unwrap();
            async move {
                receive
                    .await
                    .map_err(|_| RuntimeError::unavailable("route", "resolver ended"))
            }
            .boxed_local()
        }))
        .unwrap();
    let reference = client
        .hold(token, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    reference.release();
    assert!(resolve.send(outgoing).is_ok());
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Released);
    assert_eq!(effects.get(), 0);
    driver.run_until(client.close()).unwrap();
    driver.run_until(server.close()).unwrap();
}

#[test]
fn remote_legacy_adapter_keeps_actual_provider_fingerprint_and_validates_legacy_values() {
    let driver = ManualDriver::new();
    let server = host("server", &driver);
    let client = host("client", &driver);
    let provider = remote(
        "rpc.legacy",
        2,
        "server",
        BTreeMap::from([
            (
                "current".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
            (
                "current_method".into(),
                method(None, Some(json!({"type":"integer"}))),
            ),
        ]),
    );
    let consumer = remote(
        "rpc.legacy",
        1,
        "server",
        BTreeMap::from([
            (
                "legacy".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
            (
                "legacy_method".into(),
                method(None, Some(json!({"type":"integer"}))),
            ),
        ]),
    );
    server
        .serve(
            provider.clone(),
            Rc::new(|_, _, declaration| {
                let cell = declaration.state(
                    "value",
                    StateDeclaration::new(json!({"type":"integer"}), json!(11)),
                )?;
                Ok(Implementation::new()
                    .variant("default")
                    .state("current", cell.readable())
                    .json_method("current_method", |_, _| {
                        async { Ok(Some(json!(12))) }.boxed_local()
                    }))
            }),
            ServeOptions::default().with_adapter(
                1,
                Rc::new(|current| {
                    let state = current.clone();
                    let method = current.portable_method("current_method")?;
                    // Adaptation is an explicit view, rather than relabeling the base descriptor.
                    Ok(Implementation::new()
                        .variant("default")
                        .state("legacy", state.readable("current")?)
                        .json_method("legacy_method", move |args, ctx| method(args, ctx)))
                }),
            ),
        )
        .unwrap();
    let (_outbound, _inbound, _control) = connect(&client, &server, &driver);
    let reference = driver
        .run_until(client.get(consumer, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(reference.shape().unwrap().contract, provider.fingerprint());
    assert_eq!(
        reference
            .readable("legacy")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(11)
    );
    assert_eq!(
        driver
            .run_until(reference.call_json("legacy_method", None, None))
            .unwrap(),
        Some(json!(12))
    );
    assert_eq!(
        driver
            .run_until(reference.call_json("current_method", None, None))
            .unwrap_err()
            .code,
        ErrorCode::NotInVariant
    );
    driver.run_until(client.close()).unwrap();
    driver.run_until(server.close()).unwrap();
}

#[test]
fn cancelled_rpc_job_keeps_host_admission_and_resources_until_physical_settlement() {
    let driver = ManualDriver::new();
    let limits = HostRpcLimits {
        max_incoming_calls: 1,
        ..Default::default()
    };
    let server = Host::new(
        HostOptions::new("server", json!("server"))
            .with_rpc_limits(limits)
            .unwrap(),
        driver.clone(),
    );
    server
        .set_rpc_identity(RootOrigin::new("server", "11111111111111111111111111111111").unwrap())
        .unwrap();
    let client = host("client", &driver);
    let token = remote(
        "rpc.physical",
        1,
        "server",
        BTreeMap::from([
            ("wait".into(), method(None, None)),
            ("noop".into(), method(None, None)),
        ]),
    );
    let (started, wait_started) = oneshot::channel();
    let started = Rc::new(RefCell::new(Some(started)));
    let (finish, wait_finish) = oneshot::channel();
    let wait_finish = Rc::new(RefCell::new(Some(wait_finish)));
    let (settled, wait_settled) = oneshot::channel();
    let settled = Rc::new(RefCell::new(Some(settled)));
    let closed = Rc::new(Counter::new(false));
    let closed_factory = closed.clone();
    server
        .serve(
            token.clone(),
            Rc::new(move |_, _, declaration| {
                let closed = closed_factory.clone();
                let resource = declaration.resource(
                    "protected",
                    ResourceDeclaration::new(|_| async { Ok(1) }.boxed_local()).with_close(
                        move |_| {
                            closed.set(true);
                            async { Ok(()) }.boxed_local()
                        },
                    ),
                )?;
                let (started, wait_finish, settled) =
                    (started.clone(), wait_finish.clone(), settled.clone());
                Ok(Implementation::new()
                    .variant("default")
                    .json_method("wait", move |_, ctx| {
                        let started = started.borrow_mut().take().unwrap();
                        let finish = wait_finish.borrow_mut().take().unwrap();
                        let settled = settled.borrow_mut().take().unwrap();
                        let resource = resource.clone();
                        async move {
                            started.send(()).unwrap();
                            finish.await.unwrap();
                            assert!(ctx.cancellation().is_cancelled());
                            assert_eq!(*resource.value()?, 1);
                            settled.send(()).unwrap();
                            Ok(None)
                        }
                        .boxed_local()
                    })
                    .json_method("noop", |_, _| async { Ok(None) }.boxed_local()))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let (_outbound, _inbound, _control) = connect(&client, &server, &driver);
    let reference = driver
        .run_until(client.get(token, json!({}), GetOptions::default()))
        .unwrap();
    driver.run_until(async {
        let mut pending = Box::pin(reference.call_json("wait", None, None));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        wait_started.await.unwrap();
        drop(pending);
        let error = reference.call_json("noop", None, None).await.unwrap_err();
        assert_eq!(error.reason, "too_many_calls");
        assert!(!closed.get());
        finish.send(()).unwrap();
        wait_settled.await.unwrap();
        reference.call_json("noop", None, None).await.unwrap();
    });
    driver.run_until(client.close()).unwrap();
    driver.run_until_stalled();
    assert!(closed.get());
    driver.run_until(server.close()).unwrap();
}

#[test]
fn trusted_relay_preserves_ingress_principal_and_only_releases_its_origin_hold() {
    let driver = ManualDriver::new();
    let router = host("server", &driver);
    router.set_route_authorizer(Rc::new(|_| Ok(()))).unwrap();
    let worker = host("worker", &driver);
    let first_client = host("first-client", &driver);
    let second_client = host("second-client", &driver);
    let token = remote(
        "rpc.relay",
        1,
        "worker",
        BTreeMap::from([("who".into(), method(None, Some(json!({"type":"string"}))))]),
    );
    let factories = Rc::new(Counter::new(0));
    let created = factories.clone();
    worker
        .serve(
            token.clone(),
            Rc::new(move |_, principal, _| {
                created.set(created.get() + 1);
                Ok(Implementation::new()
                    .variant("default")
                    .json_method("who", move |_, _| {
                        let principal = principal.clone();
                        async move { Ok(Some(principal)) }.boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let (left, right, _worker_control) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let down = router
        .accept_child(
            left,
            "worker",
            json!("worker"),
            ConnectionOptions::default(),
        )
        .unwrap();
    let _up = worker
        .connect_parent(
            right,
            "server",
            json!("router"),
            ConnectionOptions::default(),
        )
        .unwrap();
    router.set_route("worker", down).unwrap();
    let mut outgoing = Vec::new();
    let mut references = Vec::new();
    for client in [&first_client, &second_client] {
        let (left, right, _) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
        let connection = client
            .connect_client(left, "server", ConnectionOptions::default())
            .unwrap();
        router
            .accept_client(right, json!("accepted-user"), ConnectionOptions::default())
            .unwrap();
        client.set_route("worker", connection.clone()).unwrap();
        references.push(
            driver
                .run_until(client.get(token.clone(), json!({}), GetOptions::default()))
                .unwrap(),
        );
        outgoing.push(connection);
    }
    assert_eq!(factories.get(), 1);
    for reference in &references {
        assert_eq!(
            driver
                .run_until(reference.call_json("who", None, None))
                .unwrap(),
            Some(json!("accepted-user"))
        );
    }
    outgoing[0].close(CloseReason::closed());
    driver.run_until_stalled();
    assert_eq!(references[1].status(), RefStatus::Live);
    assert_eq!(
        driver
            .run_until(references[1].call_json("who", None, None))
            .unwrap(),
        Some(json!("accepted-user"))
    );
    driver.run_until(first_client.close()).unwrap();
    driver.run_until(second_client.close()).unwrap();
    driver.run_until(router.close()).unwrap();
    driver.run_until(worker.close()).unwrap();
}

#[test]
fn abandoned_resolver_keeps_host_quota_until_physical_settlement() {
    let driver = ManualDriver::new();
    let client = Host::new(
        HostOptions::new("client", json!("client"))
            .with_rpc_limits(HostRpcLimits {
                max_pending_routes: 1,
                ..Default::default()
            })
            .unwrap(),
        driver.clone(),
    );
    let (finish, wait_finish) = oneshot::channel();
    let pending = Rc::new(RefCell::new(Some(wait_finish)));
    let calls = Rc::new(Counter::new(0));
    let invoked = calls.clone();
    client
        .set_resolver(Rc::new(move |_| {
            invoked.set(invoked.get() + 1);
            let pending = pending.borrow_mut().take();
            async move {
                if let Some(pending) = pending {
                    pending.await.unwrap();
                }
                Err(RuntimeError::unavailable("resolver_finished", "no route"))
            }
            .boxed_local()
        }))
        .unwrap();
    let acquire = |name| {
        client
            .hold(
                remote(name, 1, "server", BTreeMap::new()),
                json!({}),
                GetOptions::default(),
            )
            .unwrap()
    };
    let first = acquire("rpc.quota-first");
    driver.run_until_stalled();
    assert_eq!(calls.get(), 1);
    first.release();
    let second = acquire("rpc.quota-second");
    driver.run_until_stalled();
    assert_eq!(second.error().unwrap().reason, "too_many_routes");
    assert_eq!(
        calls.get(),
        1,
        "release must not free a running resolver slot"
    );
    finish.send(()).unwrap();
    driver.run_until_stalled();
    let third = acquire("rpc.quota-third");
    driver.run_until_stalled();
    assert_eq!(third.error().unwrap().reason, "resolver_finished");
    assert_eq!(calls.get(), 2, "physical completion returns admission");
    driver.run_until(client.close()).unwrap();
}

#[test]
fn abandoned_resolver_keeps_ingress_quota_without_blocking_another_connection() {
    let driver = ManualDriver::new();
    let router = host("server", &driver);
    router.set_route_authorizer(Rc::new(|_| Ok(()))).unwrap();
    let client = host("client", &driver);
    let independent = host("independent", &driver);
    let (left, right, _control) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let outbound = client
        .connect_client(left, "server", ConnectionOptions::default())
        .unwrap();
    let _incoming = router
        .accept_client(
            right,
            json!("accepted-user"),
            ConnectionOptions {
                limits: RpcLimits {
                    max_pending_routes: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    client.set_route("worker", outbound).unwrap();
    let (independent_outbound, _independent_inbound, _independent_control) =
        connect(&independent, &router, &driver);
    independent
        .set_route("worker", independent_outbound)
        .unwrap();
    let (finish, wait_finish) = oneshot::channel();
    let pending = Rc::new(RefCell::new(Some(wait_finish)));
    let calls = Rc::new(Counter::new(0));
    let invoked = calls.clone();
    router
        .set_resolver(Rc::new(move |_| {
            invoked.set(invoked.get() + 1);
            let pending = pending.borrow_mut().take();
            async move {
                if let Some(pending) = pending {
                    pending.await.unwrap();
                }
                Err(RuntimeError::unavailable("resolver_finished", "no route"))
            }
            .boxed_local()
        }))
        .unwrap();
    let acquire = |client: &Host, name| {
        client
            .hold(
                remote(name, 1, "worker", BTreeMap::new()),
                json!({}),
                GetOptions::default(),
            )
            .unwrap()
    };
    let first = acquire(&client, "rpc.ingress-first");
    driver.run_until_stalled();
    assert_eq!(calls.get(), 1);
    first.release();
    driver.run_until_stalled();
    let second = acquire(&client, "rpc.ingress-second");
    driver.run_until_stalled();
    assert_eq!(second.error().unwrap().reason, "too_many_routes");
    assert_eq!(calls.get(), 1);
    let other = acquire(&independent, "rpc.ingress-independent");
    driver.run_until_stalled();
    assert_eq!(other.error().unwrap().reason, "resolver_finished");
    assert_eq!(
        calls.get(),
        2,
        "a separate ingress retains its own allowance"
    );
    finish.send(()).unwrap();
    driver.run_until_stalled();
    let third = acquire(&client, "rpc.ingress-third");
    driver.run_until_stalled();
    assert_eq!(third.error().unwrap().reason, "resolver_finished");
    assert_eq!(calls.get(), 3);
    driver.run_until(client.close()).unwrap();
    driver.run_until(independent.close()).unwrap();
    driver.run_until(router.close()).unwrap();
}

#[test]
fn released_route_waiting_for_hello_keeps_outgoing_connection_quota() {
    let driver = ManualDriver::new();
    let server = host("server", &driver);
    let client = host("client", &driver);
    let (left, right, _control) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let outbound = client
        .connect_client(
            left,
            "server",
            ConnectionOptions {
                limits: RpcLimits {
                    max_pending_routes: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    client.set_route("server", outbound).unwrap();
    let acquire = |name| {
        client
            .hold(
                remote(name, 1, "server", BTreeMap::new()),
                json!({}),
                GetOptions::default(),
            )
            .unwrap()
    };
    let first = acquire("rpc.hello-first");
    driver.run_until_stalled();
    assert_eq!(first.status(), RefStatus::Loading);
    first.release();
    let second = acquire("rpc.hello-second");
    driver.run_until_stalled();
    assert_eq!(second.error().unwrap().reason, "too_many_routes");
    let token = remote("rpc.hello-third", 1, "server", BTreeMap::new());
    server
        .serve(
            token.clone(),
            Rc::new(|_, _, _| Ok(Implementation::new().variant("default"))),
            ServeOptions::default(),
        )
        .unwrap();
    let _incoming = server
        .accept_client(right, json!("authenticated"), ConnectionOptions::default())
        .unwrap();
    driver.run_until_stalled();
    let third = driver
        .run_until(client.get(token, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(third.status(), RefStatus::Live);
    driver.run_until(client.close()).unwrap();
    driver.run_until(server.close()).unwrap();
}

#[test]
fn host_close_keeps_held_remote_dependencies_and_trusted_roots_usable_during_cleanup() {
    let driver = ManualDriver::new();
    let server = host("server", &driver);
    let client = host("client", &driver);
    let dependency = remote(
        "rpc.cleanup-dependency",
        1,
        "server",
        BTreeMap::from([("flush".into(), method(None, None))]),
    );
    let flushes = Rc::new(Counter::new(0));
    let observed = flushes.clone();
    server
        .serve(
            dependency.clone(),
            Rc::new(move |_, _, _| {
                let observed = observed.clone();
                Ok(Implementation::new()
                    .variant("default")
                    .json_method("flush", move |_, _| {
                        observed.set(observed.get() + 1);
                        async { Ok(None) }.boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let (left, right, _control) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let outbound = client
        .connect_parent(
            left,
            "server",
            json!("parent"),
            ConnectionOptions::default(),
        )
        .unwrap();
    let inbound = server
        .accept_child(
            right,
            "client",
            json!("child"),
            ConnectionOptions::default(),
        )
        .unwrap();
    client.set_route("server", outbound.clone()).unwrap();
    let owner = local("rpc.cleanup-owner", 1, BTreeMap::new());
    let (finish_cleanup, wait_cleanup) = oneshot::channel();
    let wait_cleanup = Rc::new(RefCell::new(Some(wait_cleanup)));
    let cleanup_started = Rc::new(Counter::new(false));
    let started = cleanup_started.clone();
    let cleanup_finished = Rc::new(Counter::new(false));
    let finished = cleanup_finished.clone();
    client
        .serve(
            owner.clone(),
            Rc::new(move |_, _, declaration| {
                let dependency =
                    declaration.get(dependency.clone(), json!({}), GetOptions::default())?;
                let flush = dependency.clone();
                let finished = finished.clone();
                let started = started.clone();
                let wait_cleanup = wait_cleanup.clone();
                let mut resource = ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local())
                    .with_close(move |_| {
                        let flush = flush.clone();
                        let finished = finished.clone();
                        let wait_cleanup = wait_cleanup.borrow_mut().take().unwrap();
                        started.set(true);
                        async move {
                            wait_cleanup.await.unwrap();
                            flush.call_json("flush", None, None).await?;
                            finished.set(true);
                            Ok(())
                        }
                        .boxed_local()
                    });
                resource
                    .deps
                    .push(ResourceDependency::Reference(dependency));
                declaration.resource("buffer", resource)?;
                Ok(Implementation::new())
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let reference = driver
        .run_until(client.get(owner.clone(), json!({}), GetOptions::default()))
        .unwrap();
    assert!(client.close().now_or_never().is_none());
    driver.run_until_stalled();
    assert!(cleanup_started.get());
    assert_eq!(outbound.status(), ConnectionStatus::Open);
    assert_eq!(
        client
            .hold(owner, json!({}), GetOptions::default())
            .err()
            .unwrap()
            .reason,
        "closed",
        "closing preserves held cleanup dependencies, without new admission"
    );
    finish_cleanup.send(()).unwrap();
    driver.run_until(client.close()).unwrap();
    assert_eq!(reference.status(), RefStatus::Gone);
    assert!(cleanup_finished.get());
    assert_eq!(flushes.get(), 1);
    assert_eq!(outbound.status(), ConnectionStatus::Closed);
    driver.run_until_stalled();
    assert_eq!(inbound.status(), ConnectionStatus::Closed);
    // The dependency Host remains independently open after its client's links close.
    let independent = server
        .hold(
            remote(
                "rpc.cleanup-dependency",
                1,
                "server",
                BTreeMap::from([("flush".into(), method(None, None))]),
            ),
            json!({}),
            GetOptions::default(),
        )
        .unwrap();
    driver.run_until(independent.ready()).unwrap();
    driver.run_until(server.close()).unwrap();
}

#[test]
fn relay_requires_explicit_policy_and_never_borrows_an_incapable_egress_authority() {
    for parent in [false, true] {
        let driver = ManualDriver::new();
        let router = host("server", &driver);
        let provider = host("worker", &driver);
        let client = host("client", &driver);
        let contract = remote(
            "rpc.relay-authority",
            1,
            "worker",
            BTreeMap::from([("who".into(), method(None, Some(json!({"type":"string"}))))]),
        );
        provider
            .serve(
                contract.clone(),
                Rc::new(|_, principal, _| {
                    Ok(Implementation::new()
                        .variant("default")
                        .json_method("who", move |_, _| {
                            let principal = principal.clone();
                            async move { Ok(Some(principal)) }.boxed_local()
                        }))
                }),
                ServeOptions::default(),
            )
            .unwrap();
        let (left, right, _control) =
            memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
        let outbound = if parent {
            router
                .connect_parent(
                    left,
                    "worker",
                    json!("parent"),
                    ConnectionOptions::default(),
                )
                .unwrap()
        } else {
            router
                .connect_client(left, "worker", ConnectionOptions::default())
                .unwrap()
        };
        let _inbound = if parent {
            provider
                .accept_child(
                    right,
                    "server",
                    json!("router-authority"),
                    ConnectionOptions::default(),
                )
                .unwrap()
        } else {
            provider
                .accept_client(
                    right,
                    json!("router-authority"),
                    ConnectionOptions::default(),
                )
                .unwrap()
        };
        router.set_route("worker", outbound).unwrap();
        // A Host-owned acquisition can deliberately use its own authenticated route.
        let owned = driver
            .run_until(router.get(contract.clone(), json!({}), GetOptions::default()))
            .unwrap();
        assert_eq!(
            driver
                .run_until(owned.call_json("who", None, None))
                .unwrap(),
            Some(json!("router-authority"))
        );
        let (left, right, _client_control) =
            memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
        let client_link = client
            .connect_client(left, "server", ConnectionOptions::default())
            .unwrap();
        router
            .accept_client(right, json!("alice"), ConnectionOptions::default())
            .unwrap();
        client.set_route("worker", client_link).unwrap();
        let denied = driver
            .run_until(client.get(contract.clone(), json!({}), GetOptions::default()))
            .err()
            .expect("unconfigured relay must not grant ambient route authority");
        assert_eq!(denied.code, ErrorCode::Forbidden);
        assert_eq!(denied.reason, "relay_authorizer");
        router.set_route_authorizer(Rc::new(|_| Ok(()))).unwrap();
        let denied = driver
            .run_until(client.get(contract, json!({}), GetOptions::default()))
            .err()
            .expect("an incapable egress must not discard the forwarded caller");
        assert_eq!(denied.code, ErrorCode::Forbidden);
        assert_eq!(denied.reason, "relay_egress");
        assert_eq!(owned.status(), RefStatus::Live);
        driver.run_until(client.close()).unwrap();
        driver.run_until(router.close()).unwrap();
        driver.run_until(provider.close()).unwrap();
    }
}

struct RawProvider {
    host: Host,
    connection: Connection,
    reference: Reference,
    raw: Link,
    consumer: Rc<Contract>,
    hold: Value,
    resolutions: Rc<Counter<usize>>,
}
fn raw_send(raw: &Link, frame: Value) {
    raw.tx
        .try_send(Rc::from(serde_json::to_vec(&frame).unwrap()))
        .unwrap();
}
fn raw_next(driver: &ManualDriver, raw: &mut Link) -> Value {
    loop {
        match driver.run_until(raw.events.next()) {
            Some(LinkEvent::Message(bytes)) => {
                return contracts::parse_json(std::str::from_utf8(&bytes).unwrap()).unwrap();
            }
            Some(LinkEvent::Opened | LinkEvent::Writable) => {}
            event => panic!("expected wire message, received {event:?}"),
        }
    }
}
impl RawProvider {
    fn new(driver: &Rc<ManualDriver>, consumer: Rc<Contract>) -> Self {
        let host = host("client", driver);
        let (client, mut raw, _control) =
            memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
        let connection = host
            .connect_client(client, "server", ConnectionOptions::default())
            .unwrap();
        raw_send(
            &raw,
            json!({
                "t":"hello",
                "hello": {
                    "protocol":"tembra-rpc", "version":1, "descriptor_version":1, "delta_codec":1,
                    "peer": {"role":"host", "identity": {"host":"server", "incarnation":"11111111111111111111111111111111"}, "code_version":1},
                    "receive": {"max_message_bytes":1048576, "max_unit_bytes":4194304, "state_window_bytes":262144}
                }
            }),
        );
        driver.run_until(connection.ready()).unwrap();
        assert_eq!(raw_next(driver, &mut raw)["t"], "hello");
        let resolutions = Rc::new(Counter::new(0));
        let calls = resolutions.clone();
        let resolved = connection.clone();
        host.set_resolver(Rc::new(move |_| {
            calls.set(calls.get() + 1);
            let resolved = resolved.clone();
            async move { Ok(resolved) }.boxed_local()
        }))
        .unwrap();
        let reference = host
            .hold(consumer.clone(), json!({}), GetOptions::default())
            .unwrap();
        let hold = raw_next(driver, &mut raw);
        assert_eq!(hold["t"], "hold");
        Self {
            host,
            connection,
            reference,
            raw,
            consumer,
            hold: hold["hold"].clone(),
            resolutions,
        }
    }
    fn shape(&self, provider: &Contract, mode: Value, state: Value) {
        let members: Vec<_> = self
            .consumer
            .members("default")
            .unwrap()
            .keys()
            .cloned()
            .collect();
        raw_send(
            &self.raw,
            json!({
                "t":"shape", "hold":self.hold, "unit":1,
                "consumer_fingerprint":self.consumer.fingerprint(), "provider":provider.descriptor(),
                "mode":mode, "variant":"default", "members":members, "state":state
            }),
        );
    }
}

#[test]
fn invalid_initial_shape_is_terminal_with_the_consumer_error_before_policy_close() {
    for adapted in [false, true] {
        let driver = ManualDriver::new();
        let consumer = remote(
            "rpc.invalid-shape",
            1,
            "server",
            BTreeMap::from([("read".into(), method(None, Some(json!({"type":"integer"}))))]),
        );
        let provider = if adapted {
            consumer.clone()
        } else {
            remote(
                "rpc.invalid-shape",
                1,
                "server",
                BTreeMap::from([("read".into(), method(None, Some(json!({"type":"string"}))))]),
            )
        };
        let fixture = RawProvider::new(&driver, consumer.clone());
        let live_events = Rc::new(Counter::new(0));
        let observed = live_events.clone();
        let _status = fixture.reference.on_status(move |status, _| {
            if status == RefStatus::Live {
                observed.set(observed.get() + 1);
            }
        });
        let mode = if adapted {
            json!({"mode":"adapted", "consumer_fingerprint":consumer.fingerprint()})
        } else {
            json!({"mode":"direct"})
        };
        fixture.shape(&provider, mode, json!([]));
        driver.run_until_stalled();
        assert_eq!(live_events.get(), 0, "an invalid shape never becomes Live");
        assert_eq!(fixture.reference.status(), RefStatus::Gone);
        let error = fixture.reference.error().unwrap();
        assert_eq!(error.code, ErrorCode::ContractMismatch);
        assert_eq!(error.reason, if adapted { "adapter" } else { "coverage" });
        assert_eq!(
            driver.run_until(fixture.reference.ready()).unwrap_err(),
            error
        );
        assert_eq!(fixture.connection.status(), ConnectionStatus::Closed);
        assert_eq!(
            fixture.resolutions.get(),
            1,
            "invalid hydration is not retried through the resolver"
        );
        driver.run_until(fixture.host.close()).unwrap();
    }
}

#[test]
fn invalid_adapted_initial_state_ends_the_binding_without_publishing_or_retrying() {
    let driver = ManualDriver::new();
    let consumer = remote(
        "rpc.invalid-adapted-state",
        1,
        "server",
        BTreeMap::from([(
            "n".into(),
            Member::State {
                schema: json!({"type":"integer", "maximum":10}),
            },
        )]),
    );
    let provider = remote(
        "rpc.invalid-adapted-state",
        2,
        "server",
        BTreeMap::from([(
            "n".into(),
            Member::State {
                schema: json!({"type":"integer"}),
            },
        )]),
    );
    let fixture = RawProvider::new(&driver, consumer.clone());
    let live_events = Rc::new(Counter::new(0));
    let observed_live = live_events.clone();
    let _status = fixture.reference.on_status(move |status, _| {
        if status == RefStatus::Live {
            observed_live.set(observed_live.get() + 1);
        }
    });
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    let _subscription = fixture
        .reference
        .readable("n")
        .unwrap()
        .subscribe_events(move |event| {
            observed.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    fixture.shape(
        &provider,
        json!({"mode":"adapted", "consumer_fingerprint":consumer.fingerprint()}),
        json!([{"member":"n", "stream":1}]),
    );
    driver.run_until_stalled();
    assert_eq!(fixture.reference.status(), RefStatus::Loading);
    raw_send(
        &fixture.raw,
        json!({
            "t":"snapshot", "key":{"hold":fixture.hold, "unit":1, "member":"n", "stream":1},
            "snapshot":{"revision":0, "value":99}
        }),
    );
    driver.run_until_stalled();
    assert_eq!(fixture.reference.status(), RefStatus::Gone);
    let error = fixture.reference.error().unwrap();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (ErrorCode::Invalid, "state")
    );
    assert_eq!(
        live_events.get(),
        0,
        "invalid adapted state never becomes Live"
    );
    assert!(events.borrow().is_empty());
    assert_eq!(
        driver.run_until(fixture.reference.ready()).unwrap_err(),
        error
    );
    assert_eq!(fixture.connection.status(), ConnectionStatus::Closed);
    assert_eq!(
        fixture.resolutions.get(),
        1,
        "invalid adapted state cannot cause a hydration retry loop"
    );
    driver.run_until(fixture.host.close()).unwrap();
}

#[test]
fn invalid_remote_result_is_a_typed_call_error_while_the_binding_stays_live() {
    let driver = ManualDriver::new();
    let consumer = remote(
        "rpc.invalid-result",
        1,
        "server",
        BTreeMap::from([("read".into(), method(None, Some(json!({"type":"integer"}))))]),
    );
    let mut fixture = RawProvider::new(&driver, consumer.clone());
    fixture.shape(&consumer, json!({"mode":"direct"}), json!([]));
    driver.run_until(fixture.reference.ready()).unwrap();
    let (send, receive) = oneshot::channel();
    let reference = fixture.reference.clone();
    driver.spawn(
        async move {
            let _ = send.send(reference.call_json("read", None, None).await);
        }
        .boxed_local(),
    );
    let call = raw_next(&driver, &mut fixture.raw);
    assert_eq!(call["t"], "call");
    raw_send(
        &fixture.raw,
        json!({
            "t":"result", "call":call["call"],
            "outcome":{"status":"success", "data":{"presence":"present", "value":"invalid"}}
        }),
    );
    let error = driver.run_until(receive).unwrap().unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (ErrorCode::Invalid, "result")
    );
    assert_eq!(fixture.reference.status(), RefStatus::Live);
    assert_eq!(fixture.connection.status(), ConnectionStatus::Open);
    driver.run_until(fixture.host.close()).unwrap();
}

#[test]
fn locally_detected_protocol_policy_failure_ends_shared_views_without_resolver_retry() {
    for (frame, expected_code) in [
        (
            json!({"t":"unsupported_protocol_frame"}),
            ErrorCode::Invalid,
        ),
        (
            json!({"t":"invalidate_principal", "principal":"caller", "origin":"forged", "through_generation":0}),
            ErrorCode::Forbidden,
        ),
    ] {
        let driver = ManualDriver::new();
        let consumer = remote(
            "rpc.local-protocol-policy",
            1,
            "server",
            BTreeMap::from([("read".into(), method(None, None))]),
        );
        let fixture = RawProvider::new(&driver, consumer.clone());
        fixture.shape(&consumer, json!({"mode":"direct"}), json!([]));
        driver.run_until(fixture.reference.ready()).unwrap();
        let shared = fixture
            .host
            .hold(consumer, json!({}), GetOptions::default())
            .unwrap();
        driver.run_until(shared.ready()).unwrap();
        assert_eq!(fixture.resolutions.get(), 1);
        raw_send(&fixture.raw, frame);
        driver.run_until_stalled();
        let error = driver.run_until(fixture.connection.closed()).error;
        assert_eq!(error.code, expected_code);
        assert_eq!(fixture.reference.status(), RefStatus::Gone);
        assert_eq!(shared.status(), RefStatus::Gone);
        assert_eq!(fixture.reference.error().unwrap(), error);
        assert_eq!(shared.error().unwrap(), error);
        assert_eq!(
            fixture.resolutions.get(),
            1,
            "a locally detected policy violation must not re-hold on a protocol-invalid peer"
        );
        assert_eq!(driver.run_until(shared.ready()).unwrap_err(), error);
        driver.run_until(fixture.host.close()).unwrap();
    }
}
