use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{
    ConflictPolicy, Context, Driver, ErrorCode, Host, HostOptions, Implementation, Lifetime,
    ManualDriver, Registry, ResourceDeclaration, ServeOptions,
    contracts::{Contract, Member, ValueSchema},
};

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
            HostOptions::new("operations", json!("host")),
            driver.clone(),
        ),
        driver,
    )
}

fn contract(name: &str, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

fn portable(args: Option<Value>, result: Option<Value>) -> Member {
    Member::Method {
        args: args.map(ValueSchema::Portable),
        result: result.map(ValueSchema::Portable),
    }
}

#[test]
fn native_operation_preserves_pointer_identity_and_refuses_wrong_rust_types() {
    let (host, driver) = environment();
    let registry = Registry::new("native", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let token = Rc::new(String::from("capability"));
    let calls = Rc::new(Cell::new(0));
    let called = calls.clone();
    let provider = contract(
        "registry.native",
        BTreeMap::from([(
            "echo".into(),
            Member::Method {
                args: Some(ValueSchema::Native("capability@1".into())),
                result: Some(ValueSchema::Native("capability@1".into())),
            },
        )]),
    );
    host.serve(
        provider,
        Rc::new(move |_, _, declaration| {
            declaration.register_method(&registry, "echo", json!({"kind": "native"}), "echo")?;
            assert_eq!(
                declaration
                    .register_method(&registry, "absent", Value::Null, "absent")
                    .unwrap_err()
                    .reason,
                "registry_method"
            );
            let calls = called.clone();
            Ok(
                Implementation::new().method("echo", move |value: Rc<String>, _| {
                    calls.set(calls.get() + 1);
                    async move { Ok(value) }.boxed_local()
                }),
            )
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let operation = view
        .table(None)
        .unwrap()
        .get("echo")
        .unwrap()
        .operation()
        .unwrap()
        .clone();
    let input = token.clone();
    driver
        .run_until(host.root(move |context| {
            async move {
                let output: Rc<String> = operation.call(&context, input.clone()).await?;
                assert!(Rc::ptr_eq(&input, &output));
                let error = operation
                    .call::<u32, Rc<String>>(&context, 7)
                    .await
                    .unwrap_err();
                assert_eq!(error.reason, "native_type");
                let error = operation
                    .call::<Rc<String>, u32>(&context, input)
                    .await
                    .unwrap_err();
                assert_eq!(error.reason, "native_type");
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(
        calls.get(),
        1,
        "type refusal must happen before handler entry"
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn json_operation_preserves_absence_null_and_validates_before_dispatch() {
    let (host, driver) = environment();
    let registry = Registry::new("portable", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let calls = Rc::new(Cell::new(0));
    let called = calls.clone();
    host.serve(
        contract(
            "registry.portable",
            BTreeMap::from([
                (
                    "nullable".into(),
                    portable(
                        Some(json!({"type": ["null", "integer"]})),
                        Some(json!({"type": ["null", "integer"]})),
                    ),
                ),
                ("void".into(), portable(None, None)),
                (
                    "state".into(),
                    Member::State {
                        schema: json!({"type": "integer"}),
                    },
                ),
            ]),
        ),
        Rc::new(move |_, _, declaration| {
            declaration.register_method(&registry, "nullable", Value::Null, "nullable")?;
            declaration.register_method(&registry, "void", Value::Null, "void")?;
            assert_eq!(
                declaration
                    .register_method(&registry, "invalid", Value::Null, "state")
                    .unwrap_err()
                    .reason,
                "registry_method"
            );
            let state = declaration.state(
                "state",
                tembra_core::StateDeclaration::new(json!({"type":"integer"}), json!(0)),
            )?;
            let calls = called.clone();
            Ok(Implementation::new()
                .state("state", state.readable())
                .json_method("nullable", move |args, _| {
                    calls.set(calls.get() + 1);
                    async move { Ok(args) }.boxed_local()
                })
                .json_method("void", |args, _| {
                    assert!(args.is_none());
                    async { Ok(None) }.boxed_local()
                }))
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let table = view.table(None).unwrap();
    let nullable = table.get("nullable").unwrap().operation().unwrap().clone();
    let void = table.get("void").unwrap().operation().unwrap().clone();
    driver
        .run_until(host.root(move |context| {
            async move {
                assert_eq!(
                    nullable.call_json(&context, Some(Value::Null)).await?,
                    Some(Value::Null)
                );
                assert_eq!(
                    nullable.call_json(&context, Some(json!(7))).await?,
                    Some(json!(7))
                );
                assert_eq!(
                    nullable
                        .call_json(&context, Some(json!("bad")))
                        .await
                        .unwrap_err()
                        .reason,
                    "args"
                );
                assert_eq!(void.call_json(&context, None).await?, None);
                assert!(void.call_json(&context, Some(Value::Null)).await.is_err());
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(calls.get(), 2);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn context_authority_is_checked_at_poll_time_and_table_reads_share_the_checks() {
    let (host, driver) = environment();
    let registry = Registry::new("authority", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let called = Rc::new(Cell::new(0));
    let calls = called.clone();
    host.serve(
        contract(
            "registry.authority",
            BTreeMap::from([("run".into(), portable(None, None))]),
        ),
        Rc::new(move |_, _, declaration| {
            declaration.register_method(&registry, "run", Value::Null, "run")?;
            let calls = calls.clone();
            Ok(Implementation::new().method("run", move |(): (), _| {
                calls.set(calls.get() + 1);
                async { Ok(()) }.boxed_local()
            }))
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let operation = view
        .table(None)
        .unwrap()
        .get("run")
        .unwrap()
        .operation()
        .unwrap()
        .clone();
    let retained: Context = driver
        .run_until(host.root(|context| async move { Ok(context) }.boxed_local()))
        .unwrap();
    assert_eq!(
        driver
            .run_until(operation.call::<(), ()>(&retained, ()))
            .unwrap_err()
            .reason,
        "expired"
    );
    assert_eq!(view.table(Some(&retained)).err().unwrap().reason, "expired");
    let other = Host::new(HostOptions::new("other", Value::Null), driver.clone());
    let foreign_operation = operation.clone();
    let foreign_view = view.clone();
    driver
        .run_until(other.root(move |context| {
            async move {
                assert_eq!(
                    foreign_operation
                        .call::<(), ()>(&context, ())
                        .await
                        .unwrap_err()
                        .reason,
                    "foreign_ctx"
                );
                assert_eq!(
                    foreign_view.table(Some(&context)).err().unwrap().reason,
                    "foreign_ctx"
                );
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    let canceled_view = view.clone();
    driver
        .run_until(host.root(move |context| {
            async move {
                let unpolled = operation.call::<(), ()>(&context, ());
                context.cancel("cancel-before-dispatch");
                assert_eq!(unpolled.await.unwrap_err().code, ErrorCode::Aborted);
                assert_eq!(
                    canceled_view.table(Some(&context)).err().unwrap().code,
                    ErrorCode::Aborted
                );
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(called.get(), 0);
    driver.run_until(other.close()).unwrap();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn detached_metadata_does_not_retain_factory_or_resource_owners() {
    let (host, driver) = environment();
    let registry = Registry::new("detached", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let marker = Rc::new(());
    let weak_factory = Rc::downgrade(&marker);
    let resource_value = Rc::new(());
    let weak_resource = Rc::downgrade(&resource_value);
    host.serve(
        contract(
            "registry.detached",
            BTreeMap::from([("run".into(), portable(None, None))]),
        ),
        Rc::new(move |_, _, declaration| {
            let _capture = &marker;
            declaration.register_method(&registry, "run", json!({"saved": "metadata"}), "run")?;
            let resource_value = resource_value.clone();
            declaration.resource(
                "owned",
                ResourceDeclaration::new(move |_| {
                    let value = resource_value.clone();
                    async move { Ok(value) }.boxed_local()
                }),
            )?;
            Ok(Implementation::new().method("run", |(): (), _| async { Ok(()) }.boxed_local()))
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let snapshot = view.table(None).unwrap();
    let operation = snapshot.get("run").unwrap().operation().unwrap().clone();
    assert!(weak_factory.upgrade().is_some());
    assert!(weak_resource.upgrade().is_some());
    driver.run_until(host.close()).unwrap();
    assert!(
        weak_factory.upgrade().is_none(),
        "snapshot must not retain the factory through an Rc<ObjectRecord>"
    );
    assert!(weak_resource.upgrade().is_none());
    assert_eq!(
        snapshot.get("run").unwrap().data(),
        &json!({"saved":"metadata"})
    );
    assert_eq!(view.table(None).err().unwrap().reason, "closed");
    // A previously prepared but unpolled invocation cannot manufacture a root after close.
    let expired = driver.run_until(host.root(|context| async move { Ok(context) }.boxed_local()));
    assert_eq!(expired.err().unwrap().reason, "closed");
    drop(operation);
}

#[test]
fn admitted_operation_keeps_resource_until_physical_completion_during_close() {
    let (host, driver) = environment();
    let registry = Registry::new("physical", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let (send, receive) = oneshot::channel();
    let receiver = Rc::new(RefCell::new(Some(receive)));
    let entered = Rc::new(Cell::new(false));
    let method_entered = entered.clone();
    let closed = Rc::new(Cell::new(0));
    let resource_closed = closed.clone();
    host.serve(
        contract(
            "registry.physical",
            BTreeMap::from([(
                "wait".into(),
                portable(None, Some(json!({"type":"integer"}))),
            )]),
        ),
        Rc::new(move |_, _, declaration| {
            declaration.register_method(&registry, "wait", Value::Null, "wait")?;
            let cleaned = resource_closed.clone();
            let resource = declaration.resource(
                "physical",
                ResourceDeclaration::new(|_| async { Ok(7_u64) }.boxed_local()).with_close(
                    move |_| {
                        let cleaned = cleaned.clone();
                        async move {
                            cleaned.set(cleaned.get() + 1);
                            Ok(())
                        }
                        .boxed_local()
                    },
                ),
            )?;
            let receiver = receiver.clone();
            let entered = method_entered.clone();
            Ok(
                Implementation::new().method("wait", move |(): (), context| {
                    entered.set(true);
                    let resource = resource.clone();
                    let receiver = receiver.borrow_mut().take().unwrap();
                    async move {
                        receiver.await.unwrap();
                        assert!(context.cancellation().is_cancelled());
                        Ok(*resource.value()?)
                    }
                    .boxed_local()
                }),
            )
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let snapshot = view.table(None).unwrap();
    let operation = snapshot.get("wait").unwrap().operation().unwrap().clone();
    let running_host = host.clone();
    let (done, completed) = oneshot::channel();
    driver.spawn(
        async move {
            let result = running_host
                .root(move |context| {
                    async move { operation.call::<(), u64>(&context, ()).await }.boxed_local()
                })
                .await;
            let _ = done.send(result);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(entered.get());
    let closing = host.clone();
    let (done, finished) = oneshot::channel();
    driver.spawn(
        async move {
            let _ = done.send(closing.close().await);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert_eq!(closed.get(), 0);
    assert_eq!(snapshot.get("wait").unwrap().data(), &Value::Null);
    send.send(()).unwrap();
    assert_eq!(driver.run_until(completed).unwrap().unwrap(), 7);
    driver.run_until(finished).unwrap().unwrap();
    assert_eq!(closed.get(), 1);
}

#[test]
fn closing_refuses_new_snapshot_operations_while_held_dependency_cleanup_can_still_call() {
    let (host, driver) = environment();
    let registry = Registry::new("closing-admission", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let provider = contract(
        "registry.closing-owner",
        BTreeMap::from([(
            "run".into(),
            portable(None, Some(json!({"type":"integer"}))),
        )]),
    );
    let entries = Rc::new(Cell::new(0));
    let called = entries.clone();
    let owner_closed = Rc::new(Cell::new(0));
    let cleaned = owner_closed.clone();
    host.serve(
        provider.clone(),
        Rc::new(move |_, _, declaration| {
            declaration.register_method(&registry, "run", Value::Null, "run")?;
            let closed = cleaned.clone();
            declaration.resource(
                "owner",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        closed.set(closed.get() + 1);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            let entries = called.clone();
            Ok(Implementation::new().method("run", move |(): (), _| {
                entries.set(entries.get() + 1);
                async { Ok(7_u64) }.boxed_local()
            }))
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let operation = view
        .table(None)
        .unwrap()
        .get("run")
        .unwrap()
        .operation()
        .unwrap()
        .clone();
    let (unpark, parked) = oneshot::channel();
    let parked = Rc::new(RefCell::new(Some(parked)));
    let closer_entered = Rc::new(Cell::new(false));
    let observed = closer_entered.clone();
    let owner_observer = Rc::new(RefCell::new(None::<tembra_core::Reference>));
    let retained_owner = owner_observer.clone();
    let facet = tembra_core::NativeFacet::new(
        "closing-holder",
        vec![provider.clone()],
        move |scope, declarations| {
            let (scope, provider, parked, observed, owner_observer) = (
                scope.clone(),
                provider.clone(),
                parked.clone(),
                observed.clone(),
                retained_owner.clone(),
            );
            declarations.serve(
                contract("registry.closing-holder", BTreeMap::new()),
                Rc::new(move |_, _, declaration| {
                    let held = declaration.get(provider.clone(), json!({}), Default::default())?;
                    *owner_observer.borrow_mut() = Some(held.clone());
                    let (held, scope, parked, observed) = (
                        held.clone(),
                        scope.clone(),
                        parked.clone(),
                        observed.clone(),
                    );
                    declaration.resource(
                        "parked-closer",
                        ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                            move |_| {
                                observed.set(true);
                                let parked = parked.borrow_mut().take().unwrap();
                                let (held, scope) = (held.clone(), scope.clone());
                                async move {
                                    parked.await.unwrap();
                                    scope
                                        .root(move |context| {
                                            async move {
                                                assert_eq!(
                                                    held.call::<(), u64>("run", (), Some(&context))
                                                        .await?,
                                                    7
                                                );
                                                Ok(())
                                            }
                                            .boxed_local()
                                        })
                                        .await
                                }
                                .boxed_local()
                            },
                        ),
                    )?;
                    Ok(Implementation::new())
                }),
                eager(),
            )
        },
    )
    .unwrap();
    driver
        .run_until(host.install(facet, Default::default()).unwrap())
        .unwrap();
    let closing = host.clone();
    let (done, finished) = oneshot::channel();
    driver.spawn(
        async move {
            let _ = done.send(closing.close().await);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(
        closer_entered.get(),
        "holder cleanup must already be parked before admission is tested"
    );
    assert_eq!(
        owner_observer.borrow().as_ref().unwrap().status(),
        tembra_core::RefStatus::Live,
        "the registered owner has not reached its own disposal yet"
    );
    assert_eq!(owner_closed.get(), 0);
    driver.run_until(host.root(move |context| async move {
        let refused = operation.call::<(), u64>(&context, ()).await;
        assert!(matches!(&refused, Err(error) if error.code == ErrorCode::Unavailable && error.reason == "closed"), "fresh registry acquisition during closing must be refused: {refused:?}");
        Ok(())
    }.boxed_local())).unwrap();
    assert_eq!(
        entries.get(),
        0,
        "retained snapshot is not an already-held invocation claim"
    );
    unpark.send(()).unwrap();
    driver.run_until(finished).unwrap().unwrap();
    assert_eq!(
        entries.get(),
        1,
        "already-held dependency cleanup remains allowed"
    );
    assert_eq!(owner_closed.get(), 1);
}
