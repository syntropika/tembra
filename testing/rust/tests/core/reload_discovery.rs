use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tembra_core::{contracts::Contract, *};

fn token(name: &str) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), BTreeMap::new()).unwrap())
}
fn eager() -> ServeOptions {
    ServeOptions {
        lifetime: Lifetime::Host,
        ..Default::default()
    }
}
#[test]
fn newly_declared_host_factory_failure_aborts_before_public_service_or_epoch_adoption() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("discovery", json!(null)), driver.clone());
    let token = token("new-failing-host");
    let served = token.clone();
    let factories = Rc::new(Cell::new(0));
    let entered = factories.clone();
    let candidate = ExecutableFacet::foreign("new-failing", Vec::new(), move |_, declaration| {
        let entered = entered.clone();
        declaration.serve(
            served.clone(),
            Rc::new(move |_, _, _| {
                entered.set(entered.get() + 1);
                Err(RuntimeError::invalid(
                    "new_factory",
                    "candidate host factory refused",
                ))
            }),
            eager(),
        )
    })
    .unwrap();
    driver
        .run_until(async {
            let error = host
                .reload(vec![candidate], ReloadOptions::default())
                .await
                .unwrap_err();
            assert_eq!(error.reason, "new_factory");
            assert_eq!(factories.get(), 1);
            assert_eq!(
                host.hold(token, json!({}), GetOptions::default())
                    .err()
                    .unwrap()
                    .reason,
                "unknown_type"
            );
            let version = host
                .root(|context| async move { Ok(context.code_version()) }.boxed_local())
                .await?;
            assert_eq!(version, 1);
            host.close().await
        })
        .unwrap();
}
#[test]
fn prospective_dependency_discovery_prepares_resource_order_before_any_service_is_public() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("discovery", json!(null)), driver.clone());
    let a = token("new-consumer");
    let b = token("new-provider");
    let b_token = b.clone();
    let opened = Rc::new(Cell::new(false));
    let closed = Rc::new(Cell::new(0));
    let opens = opened.clone();
    let closes = closed.clone();
    let provider = ExecutableFacet::foreign("new-provider-facet", Vec::new(), move |_, scope| {
        let opens = opens.clone();
        let closes = closes.clone();
        scope.serve(
            b_token.clone(),
            Rc::new(move |_, _, declaration| {
                let opens = opens.clone();
                let closes = closes.clone();
                declaration.resource(
                    "provider",
                    ResourceDeclaration::new(move |_| {
                        opens.set(true);
                        async { Ok(()) }.boxed_local()
                    })
                    .with_close(move |_| {
                        closes.set(closes.get() + 1);
                        async { Ok(()) }.boxed_local()
                    }),
                )?;
                Ok(Implementation::new())
            }),
            ServeOptions::default(),
        )
    })
    .unwrap();
    let a_token = a.clone();
    let seen = opened.clone();
    let consumer = ExecutableFacet::foreign("new-consumer-facet", Vec::new(), move |_, scope| {
        let b = b.clone();
        let seen = seen.clone();
        scope.serve(
            a_token.clone(),
            Rc::new(move |_, _, declaration| {
                let dependency = declaration.get(b.clone(), json!({}), GetOptions::default())?;
                let seen = seen.clone();
                let mut resource = ResourceDeclaration::new(move |_| {
                    assert!(
                        seen.get(),
                        "provider open must physically finish before consumer open"
                    );
                    async { Ok(()) }.boxed_local()
                });
                resource
                    .deps
                    .push(ResourceDependency::Reference(dependency));
                declaration.resource("consumer", resource)?;
                Ok(Implementation::new())
            }),
            eager(),
        )
    })
    .unwrap();
    driver
        .run_until(async {
            let report = host
                .reload(vec![consumer, provider], ReloadOptions::default())
                .await?;
            assert_eq!(report.prepared_objects().len(), 2);
            assert!(opened.get());
            let reference = host.get(a, json!({}), GetOptions::default()).await?;
            assert_eq!(reference.status(), RefStatus::Live);
            reference.release();
            host.close().await?;
            assert_eq!(closed.get(), 1);
            Ok::<_, RuntimeError>(())
        })
        .unwrap();
}

#[test]
fn preparation_reaches_late_activation_fixpoint_then_rejects_only_new_changed_type_objects() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("fixpoint", json!(null)), driver.clone());
    let token = Rc::new(
        Contract::local(
            "fixpoint-owner",
            1,
            json!({"type":"object","properties":{"id":{"type":"integer"}},"required":["id"]}),
            BTreeMap::from([(
                "read".into(),
                tembra_core::contracts::Member::Method {
                    args: None,
                    result: Some(tembra_core::contracts::ValueSchema::Native(
                        "fixpoint.marker@1".into(),
                    )),
                },
            )]),
        )
        .unwrap(),
    );
    let unrelated =
        Rc::new(Contract::local("fixpoint-unrelated", 1, json!({}), BTreeMap::new()).unwrap());
    host.serve(
        unrelated.clone(),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions::default(),
    )
    .unwrap();
    let late = Rc::new(std::cell::RefCell::new(None::<Reference>));
    let add_late = Rc::new(Cell::new(true));
    let blocked = Rc::new(Cell::new(0));
    let definition = |version: u64| {
        let contract = token.clone();
        let core = host.clone();
        let late = late.clone();
        let add_late = add_late.clone();
        let blocked = blocked.clone();
        let unrelated = unrelated.clone();
        ExecutableFacet::foreign("fixpoint", Vec::new(), move |_, setup| {
            let contract_for_factory = contract.clone();
            let core = core.clone();
            let late = late.clone();
            let add_late = add_late.clone();
            let blocked = blocked.clone();
            let unrelated = unrelated.clone();
            setup.serve(
                contract.clone(),
                Rc::new(move |_, _, declaration| {
                    if version == 2 {
                        let opening_core = core.clone();
                        let contract = contract_for_factory.clone();
                        let late = late.clone();
                        let add_late = add_late.clone();
                        declaration.resource(
                            "opening",
                            ResourceDeclaration::new(move |_| {
                                let core = opening_core.clone();
                                let contract = contract.clone();
                                let late = late.clone();
                                let add_late = add_late.clone();
                                async move {
                                    if add_late.replace(false) {
                                        let reference = core.hold(
                                            contract,
                                            json!({"id":1}),
                                            GetOptions::default(),
                                        )?;
                                        reference.ready().await?;
                                        *late.borrow_mut() = Some(reference);
                                    }
                                    Ok(())
                                }
                                .boxed_local()
                            }),
                        )?;
                        let core = core.clone();
                        let contract = contract_for_factory.clone();
                        let blocked = blocked.clone();
                        let unrelated = unrelated.clone();
                        declaration.state(
                            "after-fixpoint",
                            StateDeclaration::with_initializer(
                                json!({"type":"integer"}),
                                move |context| {
                                    if context.code_version() == 1 {
                                        let error = core
                                            .hold(
                                                contract.clone(),
                                                json!({"id":2}),
                                                GetOptions::default(),
                                            )
                                            .err()
                                            .expect("new changed-type object must be gated");
                                        assert_eq!(error.reason, "upgrade_conflict");
                                        blocked.set(blocked.get() + 1);
                                    }
                                    let unrelated = core.hold(
                                        unrelated.clone(),
                                        json!({}),
                                        GetOptions::default(),
                                    );
                                    async move {
                                        let unrelated = unrelated?;
                                        unrelated.ready().await?;
                                        unrelated.release();
                                        Ok(json!(0))
                                    }
                                    .boxed_local()
                                },
                            ),
                        )?;
                    }
                    Ok(Implementation::new().method("read", move |(): (), _| {
                        async move { Ok(version) }.boxed_local()
                    }))
                }),
                ServeOptions::default(),
            )
        })
        .unwrap()
    };
    driver
        .run_until(async {
            host.install_executable(definition(1), LoadOptions::default())?
                .await?;
            let first = host
                .get(token.clone(), json!({"id":0}), GetOptions::default())
                .await?;
            let report = host
                .reload(vec![definition(2)], ReloadOptions::default())
                .await?;
            assert_eq!(report.prepared_objects().len(), 2);
            assert_eq!(blocked.get(), 2);
            assert_eq!(first.call::<(), u64>("read", (), None).await?, 2);
            let late_reference = late.borrow().as_ref().unwrap().clone();
            assert_eq!(late_reference.call::<(), u64>("read", (), None).await?, 2);
            let after = host
                .get(token.clone(), json!({"id":2}), GetOptions::default())
                .await?;
            assert_eq!(after.call::<(), u64>("read", (), None).await?, 2);
            after.release();
            first.release();
            late.borrow_mut().take().unwrap().release();
            host.close().await
        })
        .unwrap();
}

#[test]
fn new_private_dependency_method_refuses_before_effects_and_aborts_without_installed_mutation() {
    use tembra_core::contracts::{Member, ValueSchema};
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("private-dispatch", json!(null)),
        driver.clone(),
    );
    let existing = Rc::new(
        Contract::local(
            "installed-state",
            1,
            json!({}),
            BTreeMap::from([(
                "count".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            )]),
        )
        .unwrap(),
    );
    host.serve(
        existing.clone(),
        Rc::new(|_, _, declaration| {
            let cell = declaration.state(
                "count",
                StateDeclaration::new(json!({"type":"integer"}), json!(7)),
            )?;
            Ok(Implementation::new().state("count", cell.readable()))
        }),
        eager(),
    )
    .unwrap();
    let a = token("private-opener");
    let b = Rc::new(
        Contract::local(
            "private-method",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Native("private.answer@1".into())),
                },
            )]),
        )
        .unwrap(),
    );
    let effects = Rc::new(Cell::new(0));
    let closes = Rc::new(Cell::new(0));
    let b_token = b.clone();
    let observed = effects.clone();
    let cleanup = closes.clone();
    let provider = ExecutableFacet::foreign("private-provider", Vec::new(), move |_, setup| {
        let observed = observed.clone();
        let cleanup = cleanup.clone();
        setup.serve(
            b_token.clone(),
            Rc::new(move |_, _, declaration| {
                let cleanup = cleanup.clone();
                declaration.resource(
                    "opened-first",
                    ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                        move |_| {
                            cleanup.set(cleanup.get() + 1);
                            async { Ok(()) }.boxed_local()
                        },
                    ),
                )?;
                let observed = observed.clone();
                Ok(Implementation::new().method("read", move |(): (), _| {
                    observed.set(observed.get() + 1);
                    async { Ok(9_u64) }.boxed_local()
                }))
            }),
            ServeOptions::default(),
        )
    })
    .unwrap();
    let a_token = a.clone();
    let b_token = b.clone();
    let opener = ExecutableFacet::foreign("private-opener-facet", Vec::new(), move |_, setup| {
        let b_token = b_token.clone();
        setup.serve(
            a_token.clone(),
            Rc::new(move |_, _, declaration| {
                let dependency =
                    declaration.get(b_token.clone(), json!({}), GetOptions::default())?;
                let call = dependency.clone();
                let mut resource = ResourceDeclaration::new(move |_| {
                    let call = call.clone();
                    async move { call.call::<(), u64>("read", (), None).await }.boxed_local()
                });
                resource
                    .deps
                    .push(ResourceDependency::Reference(dependency));
                declaration.resource("cannot-call-private", resource)?;
                Ok(Implementation::new())
            }),
            eager(),
        )
    })
    .unwrap();
    driver
        .run_until(async {
            let installed = host.get(existing, json!({}), GetOptions::default()).await?;
            let before = driver.now();
            let error = host
                .reload(vec![opener, provider], ReloadOptions::default())
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::NotReady);
            assert_eq!(error.reason, "candidate_private");
            assert_eq!(
                driver.now(),
                before,
                "refusal must not wait for a readiness deadline"
            );
            assert_eq!(effects.get(), 0);
            assert_eq!(
                closes.get(),
                1,
                "the already opened private dependency is closed once"
            );
            assert_eq!(installed.status(), RefStatus::Live);
            assert_eq!(installed.readable("count")?.snapshot()?.value(), &json!(7));
            assert_eq!(
                host.root(|context| async move { Ok(context.code_version()) }.boxed_local())
                    .await?,
                1
            );
            assert_eq!(
                host.hold(a, json!({}), GetOptions::default())
                    .err()
                    .unwrap()
                    .reason,
                "unknown_type"
            );
            assert_eq!(
                host.hold(b, json!({}), GetOptions::default())
                    .err()
                    .unwrap()
                    .reason,
                "unknown_type"
            );
            installed.release();
            host.close().await
        })
        .unwrap();
}
