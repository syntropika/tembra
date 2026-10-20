use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::*;

fn setup() -> (Host, Rc<ManualDriver>, Rc<contracts::Contract>) {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("review", json!("host")), driver.clone());
    let contract = Rc::new(
        contracts::Contract::local("review", 1, json!({"type":"object"}), BTreeMap::new()).unwrap(),
    );
    (host, driver, contract)
}

#[test]
fn abandoned_shutdown_waiter_does_not_abandon_cleanup_or_erase_failure() {
    let (host, driver, contract) = setup();
    let (send, receive) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(receive)));
    let starts = Rc::new(Cell::new(0));
    let finishes = Rc::new(Cell::new(0));
    let (opened, closed) = (starts.clone(), finishes.clone());
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let (gate, opened, closed) = (gate.clone(), opened.clone(), closed.clone());
            declaration.resource(
                "gated",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        let receive = gate.borrow_mut().take().unwrap();
                        opened.set(opened.get() + 1);
                        let closed = closed.clone();
                        async move {
                            receive.await.unwrap();
                            closed.set(closed.get() + 1);
                            let mut error = RuntimeError::new(
                                ErrorCode::Forbidden,
                                "close_denied",
                                "close effect already happened",
                            );
                            error.set_details(json!({"attempt": 1}));
                            Err(error)
                        }
                        .boxed_local()
                    },
                ),
            )?;
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract.clone(), json!({}), GetOptions::default()))
        .unwrap();
    driver.run_until(async {
        let mut first = Box::pin(host.close());
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert_eq!(starts.get(), 1);
        drop(first);
    });
    assert!(
        host.hold(contract, json!({}), GetOptions::default())
            .is_err()
    );
    assert_eq!(reference.status(), RefStatus::Gone);
    send.send(()).unwrap();
    driver.run_until_stalled();
    assert_eq!(finishes.get(), 1);
    let error = driver.run_until(host.close()).unwrap_err();
    let nested = &error.details()["errors"][0]["details"]["errors"][0];
    assert_eq!(nested["code"], "forbidden");
    assert_eq!(nested["reason"], "close_denied");
    assert_eq!(nested["details"]["attempt"], 1);
    assert_eq!(driver.run_until(host.close()).unwrap_err(), error);
    assert_eq!(starts.get(), 1);
}

#[test]
fn principal_selection_can_reenter_host_and_cleanup_outlives_waiter() {
    let (host, driver, contract) = setup();
    let (send, receive) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(receive)));
    let finishes = Rc::new(Cell::new(0));
    let closed = finishes.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let (gate, closed) = (gate.clone(), closed.clone());
            declaration.resource(
                "gated",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        let receive = gate.borrow_mut().take();
                        let closed = closed.clone();
                        async move {
                            if let Some(receive) = receive {
                                receive.await.unwrap();
                            }
                            closed.set(closed.get() + 1);
                            Err(RuntimeError::invalid(
                                "revoke_cleanup",
                                "cleanup failed after closing",
                            ))
                        }
                        .boxed_local()
                    },
                ),
            )?;
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(
            contract.clone(),
            json!({}),
            GetOptions {
                principal: Some(json!("guest")),
                ..GetOptions::default()
            },
        ))
        .unwrap();
    let retained = RefCell::new(None);
    driver.run_until(async {
        let mut revoke = Box::pin(host.invalidate_principal(|principal| {
            // Reentrant acquisition used to panic under the registry's immutable borrow.
            *retained.borrow_mut() = Some(
                host.hold(contract.clone(), json!({}), GetOptions::default())
                    .unwrap(),
            );
            principal == &json!("guest")
        }));
        assert!(futures::poll!(revoke.as_mut()).is_pending());
        drop(revoke);
    });
    assert_eq!(reference.status(), RefStatus::Gone);
    send.send(()).unwrap();
    driver.run_until_stalled();
    assert_eq!(finishes.get(), 1);
    assert_eq!(
        host.errors()[0].details()["errors"][0]["details"]["errors"][0]["reason"],
        "revoke_cleanup"
    );
    retained.borrow_mut().take().unwrap().release();
    // The second object's close also fails; shutdown still physically completes.
    let _ = driver.run_until(host.close());
}

#[test]
fn local_named_variant_is_rejected_for_factories_and_adapter_views() {
    let (host, driver, contract) = setup();
    host.serve(
        contract.clone(),
        Rc::new(|_, _, _| Ok(Implementation::new().variant("named"))),
        ServeOptions::default(),
    )
    .unwrap();
    let error = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .err()
        .unwrap();
    assert_eq!(error.reason, "variant");
    driver.run_until(host.close()).unwrap();

    let (host, driver, legacy) = setup();
    let current = Rc::new(
        contracts::Contract::local("review", 2, json!({"type":"object"}), BTreeMap::new()).unwrap(),
    );
    host.serve(
        current.clone(),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions::default()
            .with_adapter(1, Rc::new(|_| Ok(Implementation::new().variant("named")))),
    )
    .unwrap();
    let current = driver
        .run_until(host.get(current, json!({}), GetOptions::default()))
        .unwrap();
    let legacy = host.hold(legacy, json!({}), GetOptions::default()).unwrap();
    assert_eq!(
        driver.run_until(legacy.ready()).unwrap_err().reason,
        "variant"
    );
    assert_eq!(current.status(), RefStatus::Live);
    assert!(current.shape().unwrap().members.is_empty());
    legacy.release();
    current.release();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn portable_argument_refusal_precedes_effect_but_invalid_result_follows_effect() {
    let (host, driver, _) = setup();
    let contract = Rc::new(
        contracts::Contract::local(
            "effects",
            1,
            json!({}),
            BTreeMap::from([(
                "change".into(),
                contracts::Member::Method {
                    args: Some(contracts::ValueSchema::Portable(json!({"type":"integer"}))),
                    result: Some(contracts::ValueSchema::Portable(json!({"type":"integer"}))),
                },
            )]),
        )
        .unwrap(),
    );
    let effects = Rc::new(Cell::new(0));
    let changed = effects.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, _| {
            let changed = changed.clone();
            Ok(Implementation::new().json_method("change", move |_, _| {
                changed.set(changed.get() + 1);
                async { Ok(Some(json!("invalid result"))) }.boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    assert!(
        driver
            .run_until(reference.call_json("change", Some(json!(null)), None))
            .is_err()
    );
    assert_eq!(effects.get(), 0);
    assert_eq!(
        driver
            .run_until(reference.call_json("change", Some(json!(1)), None))
            .unwrap_err()
            .reason,
        "result"
    );
    assert_eq!(effects.get(), 1);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn refused_host_lifetime_binding_leaves_provider_name_available() {
    let (host, driver, _) = setup();
    let ambiguous = Rc::new(
        contracts::Contract::local(
            "candidate",
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                contracts::Member::Method {
                    args: Some(contracts::ValueSchema::Portable(json!({"anyOf":[
                        {"type":"object","properties":{"a":{"type":"integer"}}},
                        {"type":"object","properties":{"b":{"type":"integer"}}}
                    ]}))),
                    result: None,
                },
            )]),
        )
        .unwrap(),
    );
    let invoked = Rc::new(Cell::new(0));
    let effects = invoked.clone();
    let error = host
        .serve(
            ambiguous,
            Rc::new(move |_, _, _| {
                effects.set(effects.get() + 1);
                Ok(Implementation::new())
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::ContractMismatch);
    assert_eq!(invoked.get(), 0);
    let valid =
        Rc::new(contracts::Contract::local("candidate", 1, json!({}), BTreeMap::new()).unwrap());
    host.serve(
        valid,
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    driver.run_until(host.close()).unwrap();
}

fn method_contract(version: u64, members: &[&str]) -> Rc<contracts::Contract> {
    Rc::new(
        contracts::Contract::local(
            "alias-dispatch",
            version,
            json!({}),
            members
                .iter()
                .map(|name| {
                    (
                        (*name).into(),
                        contracts::Member::Method {
                            args: Some(contracts::ValueSchema::Portable(json!({"type":"integer"}))),
                            result: Some(contracts::ValueSchema::Portable(
                                json!({"type":"integer"}),
                            )),
                        },
                    )
                })
                .collect(),
        )
        .unwrap(),
    )
}

#[test]
fn alias_preserves_both_dispatch_channels_through_a_version_adapter() {
    let (host, driver, _) = setup();
    let current = method_contract(2, &["current"]);
    let legacy = method_contract(1, &["legacy"]);
    host.serve(
        current.clone(),
        Rc::new(|_, _, _| {
            Ok(Implementation::new()
                .method::<i32, i32>("current", |value, _| {
                    async move { Ok(value + 100) }.boxed_local()
                })
                .json_method("current", |value, _| {
                    async move { Ok(Some(json!(value.unwrap().as_i64().unwrap() + 200))) }
                        .boxed_local()
                }))
        }),
        ServeOptions::default().with_adapter(
            1,
            Rc::new(|implementation| (*implementation).clone().alias_method("legacy", "current")),
        ),
    )
    .unwrap();
    driver.run_until(async {
        let provider = host
            .get(current, json!({}), GetOptions::default())
            .await
            .unwrap();
        let consumer = host
            .get(legacy, json!({}), GetOptions::default())
            .await
            .unwrap();
        assert_eq!(provider.object_id(), consumer.object_id());
        for (reference, name) in [(&provider, "current"), (&consumer, "legacy")] {
            assert_eq!(
                reference.call::<i32, i32>(name, 4, None).await.unwrap(),
                104
            );
            assert_eq!(
                reference
                    .call_json(name, Some(json!(4)), None)
                    .await
                    .unwrap(),
                Some(json!(204))
            );
        }
        host.close().await.unwrap();
    });
}

#[test]
fn alias_overwrite_removes_stale_channels_and_self_alias_keeps_the_target() {
    for (native, portable) in [(true, true), (true, false), (false, true)] {
        let (host, driver, _) = setup();
        let contract = method_contract(1, &["target", "alias"]);
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, _| {
                let mut candidate = Implementation::new()
                    .method::<i32, i32>("alias", |_, _| async { Ok(-99) }.boxed_local())
                    .json_method("alias", |_, _| async { Ok(Some(json!(-98))) }.boxed_local());
                if native {
                    candidate = candidate.method::<i32, i32>("target", |value, _| {
                        async move { Ok(value + 1) }.boxed_local()
                    });
                }
                if portable {
                    candidate = candidate.json_method("target", |value, _| {
                        async move { Ok(Some(json!(value.unwrap().as_i64().unwrap() + 2))) }
                            .boxed_local()
                    });
                }
                candidate
                    .alias_method("alias", "target")?
                    .alias_method("target", "target")
            }),
            ServeOptions::default(),
        )
        .unwrap();
        driver.run_until(async {
            let reference = host
                .get(contract, json!({}), GetOptions::default())
                .await
                .unwrap();
            for name in ["target", "alias"] {
                let result = reference.call::<i32, i32>(name, 4, None).await;
                if native {
                    assert_eq!(result.unwrap(), 5);
                } else {
                    assert_eq!(result.unwrap_err().code, ErrorCode::NotInVariant);
                }
                let result = reference.call_json(name, Some(json!(4)), None).await;
                if portable {
                    assert_eq!(result.unwrap(), Some(json!(6)));
                } else {
                    assert_eq!(result.unwrap_err().reason, "native_type");
                }
            }
            host.close().await.unwrap();
        });
    }
    assert_eq!(
        Implementation::new()
            .alias_method("alias", "missing")
            .err()
            .unwrap()
            .code,
        ErrorCode::NotInVariant
    );
}

#[test]
fn native_result_schema_is_refused_before_portable_callback_effects() {
    let (host, driver, _) = setup();
    let contract = Rc::new(
        contracts::Contract::local(
            "native-preflight",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                contracts::Member::Method {
                    args: None,
                    result: Some(contracts::ValueSchema::Native("counter@1".into())),
                },
            )]),
        )
        .unwrap(),
    );
    let effects = Rc::new(Cell::new(0));
    let value = Rc::new(Cell::new(7));
    let (observed, returned) = (effects.clone(), value.clone());
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, _| {
            let (observed, returned) = (observed.clone(), returned.clone());
            Ok(Implementation::new()
                .method("read", move |(): (), _| {
                    let returned = returned.clone();
                    async move { Ok(returned) }.boxed_local()
                })
                .json_method("read", move |_, _| {
                    observed.set(observed.get() + 1);
                    async { Ok(Some(json!(null))) }.boxed_local()
                }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    driver.run_until(async {
        let reference = host
            .get(contract, json!({}), Default::default())
            .await
            .unwrap();
        assert_eq!(
            reference
                .call_json("read", None, None)
                .await
                .unwrap_err()
                .reason,
            "native_type"
        );
        assert_eq!(effects.get(), 0);
        let result = reference
            .call::<(), Rc<Cell<i32>>>("read", (), None)
            .await
            .unwrap();
        assert!(Rc::ptr_eq(&result, &value));
        host.close().await.unwrap();
    });
}

#[test]
fn serde_result_conversion_refuses_non_finite_values_after_callback_effects() {
    let (host, driver, _) = setup();
    let nullable = json!({"anyOf": [{"type": "number"}, {"type": "null"}]});
    let contract = Rc::new(
        contracts::Contract::local(
            "finite-results",
            1,
            json!({}),
            BTreeMap::from([
                (
                    "with_args".into(),
                    contracts::Member::Method {
                        args: Some(contracts::ValueSchema::Portable(json!({"type": "integer"}))),
                        result: Some(contracts::ValueSchema::Portable(nullable.clone())),
                    },
                ),
                (
                    "without_args".into(),
                    contracts::Member::Method {
                        args: None,
                        result: Some(contracts::ValueSchema::Portable(nullable)),
                    },
                ),
            ]),
        )
        .unwrap(),
    );
    let effects = Rc::new(Cell::new(0));
    let observed = effects.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, _| {
            let with_args = observed.clone();
            let without_args = observed.clone();
            Ok(Implementation::new()
                .serde_method("with_args", move |kind: u8, _| {
                    with_args.set(with_args.get() + 1);
                    async move {
                        Ok(Some(match kind {
                            0 => f64::NAN,
                            1 => f64::INFINITY,
                            _ => f64::NEG_INFINITY,
                        }))
                    }
                    .boxed_local()
                })
                .serde_no_args("without_args", move |_| {
                    without_args.set(without_args.get() + 1);
                    async { Ok(Some(f64::INFINITY)) }.boxed_local()
                }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    driver.run_until(async {
        let reference = host
            .get(contract, json!({}), Default::default())
            .await
            .unwrap();
        for kind in 0..3 {
            assert_eq!(
                reference
                    .call_json("with_args", Some(json!(kind)), None)
                    .await
                    .unwrap_err()
                    .reason,
                "result"
            );
        }
        assert_eq!(
            reference
                .call_json("without_args", None, None)
                .await
                .unwrap_err()
                .reason,
            "result"
        );
        assert_eq!(
            effects.get(),
            4,
            "conversion failure does not undo handler effects"
        );
        host.close().await.unwrap();
    });
}

#[test]
fn readiness_timeouts_release_calling_claim_clones_but_not_independent_claims() {
    for portable in [false, true] {
        let driver = ManualDriver::new();
        let mut options = HostOptions::new("timeout", json!("host"));
        options.ready_timeout = std::time::Duration::from_millis(1);
        let host = Host::new(options, driver.clone());
        let contract = method_contract(1, &["read"]);
        let (send, receive) = oneshot::channel();
        let gate = Rc::new(RefCell::new(Some(receive)));
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let gate = gate.clone();
                declaration.resource(
                    "blocked",
                    ResourceDeclaration::new(move |_| {
                        let receive = gate.borrow_mut().take().unwrap();
                        async move {
                            receive.await.unwrap();
                            Ok(())
                        }
                        .boxed_local()
                    }),
                )?;
                Ok(Implementation::new()
                    .method("read", |value: i32, _| {
                        async move { Ok(value) }.boxed_local()
                    })
                    .json_method("read", |value, _| async move { Ok(value) }.boxed_local()))
            }),
            Default::default(),
        )
        .unwrap();
        let timed = host
            .hold(contract.clone(), json!({}), Default::default())
            .unwrap();
        let clone = timed.clone();
        let independent = host.hold(contract, json!({}), Default::default()).unwrap();
        let error = if portable {
            driver
                .run_until(timed.call_json("read", Some(json!(3)), None))
                .unwrap_err()
        } else {
            driver
                .run_until(timed.call::<i32, i32>("read", 3, None))
                .unwrap_err()
        };
        assert_eq!(error.reason, "ready_timeout");
        assert_eq!(timed.status(), RefStatus::Released);
        assert_eq!(clone.status(), RefStatus::Released);
        assert_eq!(independent.status(), RefStatus::Loading);
        send.send(()).unwrap();
        driver.run_until_stalled();
        assert_eq!(independent.status(), RefStatus::Live);
        assert_eq!(
            driver
                .run_until(independent.call::<i32, i32>("read", 3, None))
                .unwrap(),
            3
        );
        driver.run_until(host.close()).unwrap();
    }
}
