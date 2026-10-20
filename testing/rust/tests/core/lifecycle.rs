use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};
use tembra_contracts::{Contract, Member, ValueSchema};
use tembra_core::*;

fn contract(name: &str, methods: &[&str]) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            1,
            json!({"type":"object","properties":{"id":{"type":"integer","default":0}}}),
            methods
                .iter()
                .map(|name| {
                    (
                        (*name).into(),
                        Member::Method {
                            args: Some(ValueSchema::Native("test.args@1".into())),
                            result: Some(ValueSchema::Native("test.result@1".into())),
                        },
                    )
                })
                .collect(),
        )
        .unwrap(),
    )
}

fn host() -> (Host, Rc<ManualDriver>) {
    let driver = ManualDriver::new();
    (
        Host::new(
            HostOptions::new("test", json!({"kind":"host"})),
            driver.clone(),
        ),
        driver,
    )
}

#[test]
fn identity_claims_native_values_and_retry_after_disposal() {
    struct Native(u64);
    let (host, driver) = host();
    let contract = contract("counter", &["echo"]);
    let count = Rc::new(Cell::new(0));
    let created = count.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, _| {
            created.set(created.get() + 1);
            Ok(
                Implementation::new().method::<Native, Native>("echo", |value, _| {
                    async move { Ok(value) }.boxed_local()
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let first = host
        .hold(contract.clone(), json!({}), GetOptions::default())
        .unwrap();
    let second = host
        .hold(
            contract.clone(),
            json!({"id":0,"unknown":true}),
            GetOptions::default(),
        )
        .unwrap();
    assert_eq!(first.status(), RefStatus::Loading);
    assert_eq!(first.object_id(), second.object_id());
    driver.run_until(async {
        first.ready().await.unwrap();
        assert_eq!(
            first
                .call::<Native, Native>("echo", Native(9), None)
                .await
                .unwrap()
                .0,
            9
        );
        assert_eq!(
            first
                .call::<u8, Native>("echo", 1, None)
                .await
                .err()
                .unwrap()
                .reason,
            "native_type"
        );
    });
    first.release();
    first.release();
    assert_eq!(first.status(), RefStatus::Released);
    assert_eq!(second.status(), RefStatus::Live);
    assert_eq!(count.get(), 1);
    let id = second.object_id();
    second.release();
    driver.run_until_stalled();
    let third = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until(third.ready()).unwrap();
    assert_ne!(third.object_id(), id);
    assert_eq!(count.get(), 2);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn declared_references_deduplicate_and_cycles_fail_with_original_error() {
    let (host, driver) = host();
    let a = contract("a", &[]);
    let b = contract("b", &[]);
    let target = b.clone();
    host.serve(
        a.clone(),
        Rc::new(move |_, _, decl| {
            let one = decl.get(target.clone(), json!({}), GetOptions::default())?;
            let two = decl.get(target.clone(), json!({}), GetOptions::default())?;
            assert_eq!(one.object_id(), two.object_id());
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let target = a.clone();
    host.serve(
        b,
        Rc::new(move |_, _, decl| {
            let _ = decl.get(target.clone(), json!({}), GetOptions::default())?;
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host.hold(a, json!({}), GetOptions::default()).unwrap();
    let error = driver.run_until(reference.ready()).unwrap_err();
    assert_eq!(error.code, ErrorCode::Invalid);
    assert_eq!(error.reason, "cycle");
    driver.run_until(host.close()).unwrap();
}

#[test]
fn resource_dag_and_cleanup_reverse_actual_open_order() {
    let (host, driver) = host();
    let contract = contract("resource", &["read"]);
    let events = Rc::new(RefCell::new(Vec::new()));
    let log = events.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, decl| {
            let open_log = log.clone();
            let close_log = log.clone();
            let one = decl.resource(
                "one",
                ResourceDeclaration::new(move |_| {
                    let log = open_log.clone();
                    async move {
                        log.borrow_mut().push("open one");
                        Ok(10_u32)
                    }
                    .boxed_local()
                })
                .with_close(move |_| {
                    let log = close_log.clone();
                    async move {
                        log.borrow_mut().push("close one");
                        Err(RuntimeError::unavailable("close", "one failed"))
                    }
                    .boxed_local()
                }),
            )?;
            assert_eq!(one.value().unwrap_err().code, ErrorCode::NotReady);
            let open_log = log.clone();
            let close_log = log.clone();
            let first = one.clone();
            let mut declaration = ResourceDeclaration::new(move |_| {
                let log = open_log.clone();
                let first = first.clone();
                async move {
                    assert_eq!(*first.value()?, 10);
                    log.borrow_mut().push("open two");
                    Ok(20_u32)
                }
                .boxed_local()
            })
            .with_close(move |_| {
                let log = close_log.clone();
                async move {
                    log.borrow_mut().push("close two");
                    Ok(())
                }
                .boxed_local()
            });
            declaration.deps.push(one.dependency());
            let two = decl.resource("two", declaration)?;
            Ok(
                Implementation::new().method::<(), u32>("read", move |_, _| {
                    let two = two.clone();
                    async move { Ok(*two.value()?) }.boxed_local()
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    assert_eq!(
        driver
            .run_until(reference.call::<(), u32>("read", (), None))
            .unwrap(),
        20
    );
    reference.release();
    driver.run_until_stalled();
    assert_eq!(
        *events.borrow(),
        ["open one", "open two", "close two", "close one"]
    );
    assert_eq!(host.errors().len(), 1);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn last_release_and_forced_revocation_wait_for_active_method() {
    for forced in [false, true] {
        let (host, driver) = host();
        let contract = contract("active", &["wait"]);
        let (send, receive) = oneshot::channel::<()>();
        let receive = Rc::new(RefCell::new(Some(receive)));
        let closed = Rc::new(Cell::new(false));
        let close = closed.clone();
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, decl| {
                let close = close.clone();
                let resource = decl.resource(
                    "native",
                    ResourceDeclaration::new(|_| async { Ok(7_u64) }.boxed_local()).with_close(
                        move |_| {
                            let close = close.clone();
                            async move {
                                close.set(true);
                                Ok(())
                            }
                            .boxed_local()
                        },
                    ),
                )?;
                let receiver = receive.clone();
                Ok(
                    Implementation::new().method::<(), bool>("wait", move |_, ctx| {
                        let resource = resource.clone();
                        let receive = receiver.borrow_mut().take().unwrap();
                        async move {
                            receive.await.unwrap();
                            assert_eq!(*resource.value()?, 7);
                            Ok(ctx.cancellation().is_cancelled())
                        }
                        .boxed_local()
                    }),
                )
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let reference = host
            .hold(
                contract,
                json!({}),
                GetOptions {
                    principal: Some(json!({"guest":1})),
                    ..GetOptions::default()
                },
            )
            .unwrap();
        driver.run_until(reference.ready()).unwrap();
        let task_ref = reference.clone();
        let (done, finished) = oneshot::channel();
        driver.spawn(
            async move {
                let _ = done.send(task_ref.call::<(), bool>("wait", (), None).await);
            }
            .boxed_local(),
        );
        driver.run_until_stalled();
        reference.release();
        let revoke = if forced {
            let host = host.clone();
            let (done, finished) = oneshot::channel();
            driver.spawn(
                async move {
                    let _ = done.send(host.invalidate_principal(|_| true).await);
                }
                .boxed_local(),
            );
            Some(finished)
        } else {
            None
        };
        driver.run_until_stalled();
        assert!(!closed.get());
        send.send(()).unwrap();
        assert_eq!(driver.run_until(finished).unwrap().unwrap(), forced);
        if let Some(revoke) = revoke {
            driver.run_until(revoke).unwrap().unwrap();
        }
        driver.run_until_stalled();
        assert!(closed.get());
        driver.run_until(host.close()).unwrap();
    }
}

#[test]
fn abandoned_activation_closes_late_value_and_next_get_waits_for_disposal() {
    let (host, driver) = host();
    let contract = contract("late", &[]);
    let (send, receive) = oneshot::channel::<()>();
    let receive = Rc::new(RefCell::new(Some(receive)));
    let opens = Rc::new(Cell::new(0));
    let closes = Rc::new(Cell::new(0));
    let opened = opens.clone();
    let closed = closes.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, decl| {
            let receive = receive.clone();
            let opened = opened.clone();
            let closed = closed.clone();
            decl.resource(
                "late",
                ResourceDeclaration::new(move |_| {
                    let receive = receive.borrow_mut().take();
                    let opened = opened.clone();
                    async move {
                        if let Some(receive) = receive {
                            let _ = receive.await;
                        }
                        opened.set(opened.get() + 1);
                        Ok(())
                    }
                    .boxed_local()
                })
                .with_close(move |_| {
                    let closed = closed.clone();
                    async move {
                        closed.set(closed.get() + 1);
                        Ok(())
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let first = host
        .hold(contract.clone(), json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    first.release();
    let second = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    send.send(()).unwrap();
    driver.run_until(second.ready()).unwrap();
    assert_ne!(first.object_id(), second.object_id());
    assert_eq!(opens.get(), 2);
    assert_eq!(closes.get(), 1);
    driver.run_until(host.close()).unwrap();
    assert_eq!(closes.get(), 2);
}

#[test]
fn linger_reacquisition_preserves_object_and_host_lifetime_has_pin() {
    let (host, driver) = host();
    let linger_contract = contract("linger", &[]);
    host.serve(
        linger_contract.clone(),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions {
            lifetime: Lifetime::Referenced,
            linger: Duration::from_secs(5),
            ..Default::default()
        },
    )
    .unwrap();
    let first = host
        .hold(linger_contract.clone(), json!({}), GetOptions::default())
        .unwrap();
    driver.run_until(first.ready()).unwrap();
    let id = first.object_id();
    first.release();
    driver.run_until_stalled();
    driver.advance(Duration::from_secs(2));
    let second = host
        .hold(linger_contract.clone(), json!({}), GetOptions::default())
        .unwrap();
    assert_eq!(second.object_id(), id);
    second.release();
    driver.run_until_stalled();
    driver.advance(Duration::from_secs(6));
    driver.run_until_stalled();
    let third = host
        .hold(linger_contract, json!({}), GetOptions::default())
        .unwrap();
    assert_ne!(third.object_id(), id);
    let pinned = contract("pinned", &[]);
    host.serve(
        pinned.clone(),
        Rc::new(|_, principal, _| {
            assert_eq!(principal, json!({"kind":"host"}));
            Ok(Implementation::new())
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let a = host
        .hold(
            pinned.clone(),
            json!({}),
            GetOptions {
                principal: Some(json!("guest")),
                ..GetOptions::default()
            },
        )
        .unwrap();
    let id = a.object_id();
    a.release();
    driver.run_until_stalled();
    let b = host
        .hold(pinned.clone(), json!({}), GetOptions::default())
        .unwrap();
    assert_eq!(b.object_id(), id);
    assert!(
        host.hold(pinned, json!({"id":1}), GetOptions::default())
            .is_err()
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn cleanup_keeps_dependencies_usable_and_shutdown_orders_referrers_first() {
    let (host, driver) = host();
    let target = contract("target", &["read"]);
    let owner = contract("owner", &[]);
    let events = Rc::new(RefCell::new(Vec::new()));
    let log = events.clone();
    host.serve(
        target.clone(),
        Rc::new(move |_, _, decl| {
            let log = log.clone();
            decl.resource(
                "target",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        let log = log.clone();
                        async move {
                            log.borrow_mut().push("target");
                            Ok(())
                        }
                        .boxed_local()
                    },
                ),
            )?;
            Ok(Implementation::new()
                .method::<(), u32>("read", |_, _| async { Ok(17) }.boxed_local()))
        }),
        Default::default(),
    )
    .unwrap();
    let log = events.clone();
    host.serve(
        owner.clone(),
        Rc::new(move |_, _, decl| {
            let reference = decl.get(target.clone(), json!({}), Default::default())?;
            let log = log.clone();
            let mut resource = ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local())
                .with_close(move |_| {
                    let reference = reference.clone();
                    let log = log.clone();
                    async move {
                        assert_eq!(reference.call::<(), u32>("read", (), None).await?, 17);
                        log.borrow_mut().push("owner");
                        Ok(())
                    }
                    .boxed_local()
                });
            resource.deps.push(
                decl.get(target.clone(), json!({}), Default::default())?
                    .dependency(),
            );
            decl.resource("owner", resource)?;
            Ok(Implementation::new())
        }),
        Default::default(),
    )
    .unwrap();
    let reference = host.hold(owner, json!({}), Default::default()).unwrap();
    driver.run_until(reference.ready()).unwrap();
    driver.run_until(host.close()).unwrap();
    assert_eq!(*events.borrow(), ["owner", "target"]);
}

#[test]
fn native_opaque_resource_and_loading_placeholder_rules() {
    let (host, driver) = host();
    let opaque = Rc::new(
        Contract::local(
            "opaque",
            1,
            json!({"type":"object","properties":{}}),
            BTreeMap::from([(
                "native".into(),
                Member::Opaque {
                    id: "native@1".into(),
                },
            )]),
        )
        .unwrap(),
    );
    host.serve(
        opaque.clone(),
        Rc::new(|_, _, decl| {
            let resource = decl.resource(
                "native",
                ResourceDeclaration::new(|_| async { Ok(Cell::new(13_u32)) }.boxed_local()),
            )?;
            Ok(Implementation::new().opaque("native", resource))
        }),
        Default::default(),
    )
    .unwrap();
    let reference = host.hold(opaque, json!({}), Default::default()).unwrap();
    assert!(reference.opaque::<Cell<u32>>("native").is_err());
    driver.run_until(reference.ready()).unwrap();
    let native = reference.opaque::<Cell<u32>>("native").unwrap();
    native.set(27);
    assert_eq!(reference.opaque::<Cell<u32>>("native").unwrap().get(), 27);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn absent_arguments_require_unit_and_void_results_are_discarded() {
    let (host, driver) = host();
    let void = Rc::new(
        Contract::local(
            "void",
            1,
            json!({"type":"object","properties":{}}),
            BTreeMap::from([(
                "invoke".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            )]),
        )
        .unwrap(),
    );
    host.serve(
        void.clone(),
        Rc::new(|_, _, _| {
            Ok(Implementation::new()
                .method::<(), u32>("invoke", |_, _| async { Ok(11) }.boxed_local()))
        }),
        Default::default(),
    )
    .unwrap();
    let reference = host.hold(void, json!({}), Default::default()).unwrap();
    driver
        .run_until(reference.call::<(), ()>("invoke", (), None))
        .unwrap();
    assert_eq!(
        driver
            .run_until(reference.call::<(), u32>("invoke", (), None))
            .unwrap_err()
            .reason,
        "native_type"
    );
    let invalid = Rc::new(
        Contract::local(
            "badvoid",
            1,
            json!({"type":"object","properties":{}}),
            BTreeMap::from([(
                "invoke".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            )]),
        )
        .unwrap(),
    );
    host.serve(
        invalid.clone(),
        Rc::new(|_, _, _| {
            Ok(Implementation::new()
                .method::<u32, u32>("invoke", |n, _| async move { Ok(n) }.boxed_local()))
        }),
        Default::default(),
    )
    .unwrap();
    let bad = host.hold(invalid, json!({}), Default::default()).unwrap();
    assert_eq!(
        driver.run_until(bad.ready()).unwrap_err().reason,
        "implementation"
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn readiness_timeout_releases_only_its_claim_and_status_unsubscribes() {
    let (host, driver) = host();
    let contract = contract("waitready", &[]);
    let (send, receive) = oneshot::channel::<()>();
    let receive = Rc::new(RefCell::new(Some(receive)));
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, decl| {
            let receive = receive.clone();
            decl.resource(
                "delayed",
                ResourceDeclaration::new(move |_| {
                    let receive = receive.borrow_mut().take().unwrap();
                    async move {
                        let _ = receive.await;
                        Ok(())
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new())
        }),
        Default::default(),
    )
    .unwrap();
    let first = host
        .hold(contract.clone(), json!({}), Default::default())
        .unwrap();
    let second = host.hold(contract, json!({}), Default::default()).unwrap();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let events = seen.clone();
    let stop = first.on_status(move |status, _| events.borrow_mut().push(status));
    assert_eq!(
        driver.run_until(first.ready()).unwrap_err().reason,
        "ready_timeout"
    );
    assert_eq!(first.status(), RefStatus::Released);
    assert_eq!(second.status(), RefStatus::Loading);
    stop();
    send.send(()).unwrap();
    driver.run_until(second.ready()).unwrap();
    assert_eq!(*seen.borrow(), [RefStatus::Released]);
    assert!(Rc::ptr_eq(second.contract(), &second.contract().clone()));
    driver.run_until(host.close()).unwrap();
}

#[test]
fn failed_activation_preserves_cause_and_reports_cleanup_failure() {
    let (host, driver) = host();
    let contract = contract("failed-open", &[]);
    host.serve(
        contract.clone(),
        Rc::new(|_, _, decl| {
            let first = decl.resource(
                "first",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(|_| {
                    async { Err(RuntimeError::unavailable("cleanup_failure", "close failed")) }
                        .boxed_local()
                }),
            )?;
            let mut second = ResourceDeclaration::<()>::new(|_| {
                async {
                    Err(RuntimeError::new(
                        ErrorCode::Forbidden,
                        "open_denied",
                        "resource refused",
                    ))
                }
                .boxed_local()
            });
            second.deps.push(first.dependency());
            decl.resource("second", second)?;
            Ok(Implementation::new())
        }),
        Default::default(),
    )
    .unwrap();
    let reference = host.hold(contract, json!({}), Default::default()).unwrap();
    let error = driver.run_until(reference.ready()).unwrap_err();
    assert_eq!(error.code, ErrorCode::Forbidden);
    assert_eq!(error.reason, "open_denied");
    driver.run_until_stalled();
    assert_eq!(host.errors().len(), 1);
    assert!(
        host.errors()[0]
            .details()
            .to_string()
            .contains("cleanup_failure")
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn explicit_close_releases_factory_captures_including_host_handles() {
    let (host, driver) = host();
    let sentinel = Rc::new(());
    let weak = Rc::downgrade(&sentinel);
    let captured_host = host.clone();
    host.serve(
        contract("captures", &[]),
        Rc::new(move |_, _, _| {
            let _ = (&sentinel, captured_host.id());
            Ok(Implementation::new())
        }),
        Default::default(),
    )
    .unwrap();
    assert!(weak.upgrade().is_some());
    driver.run_until(host.close()).unwrap();
    assert!(weak.upgrade().is_none());
}
