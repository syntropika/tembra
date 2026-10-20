use futures::FutureExt;
use serde_json::{Value, json};
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tembra_core::{
    contracts::{Contract, Member, Placement, ValueSchema},
    *,
};

fn method(result: Value) -> Member {
    Member::Method {
        args: None,
        result: Some(ValueSchema::Portable(result)),
    }
}
fn token(name: &str, version: u64, member: &str, input: Value, result: Value) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            version,
            input,
            BTreeMap::from([(member.into(), method(result))]),
        )
        .unwrap(),
    )
}
fn serve(
    name: &str,
    contract: Rc<Contract>,
    member: &str,
    value: i64,
    options: ServeOptions,
    count: Rc<Cell<usize>>,
) -> ExecutableFacet {
    let member = member.to_owned();
    ExecutableFacet::foreign(name, Vec::new(), move |_, setup| {
        let count = count.clone();
        let member = member.clone();
        setup.serve(
            contract.clone(),
            Rc::new(move |_, _, _| {
                count.set(count.get() + 1);
                Ok(
                    Implementation::new().json_method(member.clone(), move |_, _| {
                        async move { Ok(Some(json!(value))) }.boxed_local()
                    }),
                )
            }),
            options.clone(),
        )
    })
    .unwrap()
}
fn host() -> (Host, Rc<ManualDriver>) {
    let driver = ManualDriver::new();
    (
        Host::new(HostOptions::new("contracts", json!(null)), driver.clone()),
        driver,
    )
}
#[test]
fn same_version_forgotten_bump_and_new_version_without_adapter_preserve_the_live_provider() {
    for (version, member) in [(1, "renamed"), (2, "renamed")] {
        let (host, driver) = host();
        let old = token(
            "contract-owner",
            1,
            "read",
            json!({}),
            json!({"type":"integer"}),
        );
        let candidate = token(
            "contract-owner",
            version,
            member,
            json!({}),
            json!({"type":"integer"}),
        );
        let entered = Rc::new(Cell::new(0));
        let initial = serve(
            "provider",
            old.clone(),
            "read",
            1,
            ServeOptions::default(),
            Rc::new(Cell::new(0)),
        );
        let replacement = serve(
            "provider",
            candidate,
            member,
            2,
            ServeOptions::default(),
            entered.clone(),
        );
        driver
            .run_until(async {
                host.install_executable(initial, LoadOptions::default())?
                    .await?;
                let reference = host
                    .get(old.clone(), json!({}), GetOptions::default())
                    .await?;
                let error = host
                    .reload(vec![replacement], ReloadOptions::default())
                    .await
                    .unwrap_err();
                assert_eq!(error.code, ErrorCode::ContractMismatch);
                // Forgotten-bump checks precede recording. Surviving cross-version references
                // are checked after recording; arbitrary factory effects are not rolled back.
                assert_eq!(entered.get(), if version == 1 { 0 } else { 1 });
                assert_eq!(
                    reference.call_json("read", None, None).await?,
                    Some(json!(1))
                );
                let epoch = host
                    .root(|context| async move { Ok(context.code_version()) }.boxed_local())
                    .await?;
                assert_eq!(epoch, 1);
                reference.release();
                host.close().await
            })
            .unwrap();
    }
}
#[test]
fn changed_identity_defaults_are_rejected_before_factory_and_preserve_address() {
    let (host, driver) = host();
    let input =
        |value| json!({"type":"object","properties":{"id":{"type":"integer","default":value}}});
    let old = token(
        "identity-owner",
        1,
        "read",
        input(1),
        json!({"type":"integer"}),
    );
    let candidate = token(
        "identity-owner",
        1,
        "read",
        input(2),
        json!({"type":"integer"}),
    );
    let entered = Rc::new(Cell::new(0));
    driver
        .run_until(async {
            host.install_executable(
                serve(
                    "identity",
                    old.clone(),
                    "read",
                    1,
                    ServeOptions::default(),
                    Rc::new(Cell::new(0)),
                ),
                LoadOptions::default(),
            )?
            .await?;
            let reference = host
                .get(old.clone(), json!({}), GetOptions::default())
                .await?;
            let id = reference.object_id();
            let error = host
                .reload(
                    vec![serve(
                        "identity",
                        candidate,
                        "read",
                        2,
                        ServeOptions::default(),
                        entered.clone(),
                    )],
                    ReloadOptions::default(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ContractMismatch);
            assert_eq!(entered.get(), 0);
            let again = host.get(old, json!({}), GetOptions::default()).await?;
            assert_eq!(again.object_id(), id);
            assert_eq!(
                reference.call_json("read", None, None).await?,
                Some(json!(1))
            );
            reference.release();
            again.release();
            host.close().await
        })
        .unwrap();
}
#[test]
fn placement_change_rejects_even_when_both_profiles_resolve_to_this_host() {
    let (host, driver) = host();
    let input = json!({
        "type": "object",
        "properties": {"suffix": {"type": "string", "default": "s"}},
        "required": ["suffix"]
    });
    let variants = BTreeMap::from([(
        "v".into(),
        BTreeMap::from([("read".into(), method(json!({"type":"integer"})))]),
    )]);
    let old = Rc::new(
        Contract::remote(
            "placement-owner",
            1,
            input.clone(),
            Placement::Named {
                name: "contracts".into(),
            },
            variants.clone(),
        )
        .unwrap(),
    );
    let candidate = Rc::new(
        Contract::remote(
            "placement-owner",
            1,
            input,
            Placement::Family {
                prefix: "contract".into(),
                key: "suffix".into(),
            },
            variants,
        )
        .unwrap(),
    );
    let facet = |contract: Rc<Contract>| {
        ExecutableFacet::foreign("placement", Vec::new(), move |_, setup| {
            setup.serve(
                contract.clone(),
                Rc::new(|_, _, _| {
                    Ok(Implementation::new()
                        .variant("v")
                        .json_method("read", |_, _| async { Ok(Some(json!(1))) }.boxed_local()))
                }),
                ServeOptions::default(),
            )
        })
        .unwrap()
    };
    driver
        .run_until(async {
            host.install_executable(facet(old.clone()), LoadOptions::default())?
                .await?;
            let reference = host.get(old, json!({}), GetOptions::default()).await?;
            assert_eq!(
                host.reload(vec![facet(candidate)], ReloadOptions::default())
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::ContractMismatch
            );
            assert_eq!(
                reference.call_json("read", None, None).await?,
                Some(json!(1))
            );
            reference.release();
            host.close().await
        })
        .unwrap();
}
#[test]
fn retained_consumer_uses_explicit_adapter_after_commit_and_keeps_its_token() {
    let (host, driver) = host();
    let old = token(
        "adapted-owner",
        1,
        "old",
        json!({}),
        json!({"type":"integer"}),
    );
    let current = token(
        "adapted-owner",
        2,
        "new",
        json!({}),
        json!({"type":"integer"}),
    );
    let options = ServeOptions::default().with_adapter(
        1,
        Rc::new(|implementation| (*implementation).clone().alias_method("old", "new")),
    );
    driver
        .run_until(async {
            host.install_executable(
                serve(
                    "adapted",
                    old.clone(),
                    "old",
                    1,
                    ServeOptions::default(),
                    Rc::new(Cell::new(0)),
                ),
                LoadOptions::default(),
            )?
            .await?;
            let reference = host
                .get(old.clone(), json!({}), GetOptions::default())
                .await?;
            let id = reference.object_id();
            host.reload(
                vec![serve(
                    "adapted",
                    current.clone(),
                    "new",
                    2,
                    options,
                    Rc::new(Cell::new(0)),
                )],
                ReloadOptions::default(),
            )
            .await?;
            assert_eq!(reference.contract().fingerprint(), old.fingerprint());
            assert_eq!(
                reference.call_json("old", None, None).await?,
                Some(json!(2))
            );
            let newer = host.get(current, json!({}), GetOptions::default()).await?;
            assert_eq!(newer.object_id(), id);
            assert_eq!(newer.call_json("new", None, None).await?, Some(json!(2)));
            reference.release();
            newer.release();
            host.close().await
        })
        .unwrap();
}
#[test]
fn host_lifecycle_pin_retokens_without_a_consumer_adapter_or_new_instance() {
    let (host, driver) = host();
    let old = token("pin-owner", 1, "old", json!({}), json!({"type":"integer"}));
    let current = token("pin-owner", 2, "new", json!({}), json!({"type":"integer"}));
    let options = ServeOptions {
        lifetime: Lifetime::Host,
        ..Default::default()
    };
    let entered = Rc::new(Cell::new(0));
    driver
        .run_until(async {
            host.install_executable(
                serve("pin", old, "old", 1, options.clone(), Rc::new(Cell::new(0))),
                LoadOptions::default(),
            )?
            .await?;
            host.reload(
                vec![serve(
                    "pin",
                    current.clone(),
                    "new",
                    2,
                    options,
                    entered.clone(),
                )],
                ReloadOptions::default(),
            )
            .await?;
            let reference = host
                .get(current.clone(), json!({}), GetOptions::default())
                .await?;
            let id = reference.object_id();
            assert_eq!(
                reference.call_json("new", None, None).await?,
                Some(json!(2))
            );
            reference.release();
            let again = host.get(current, json!({}), GetOptions::default()).await?;
            assert_eq!(again.object_id(), id);
            assert_eq!(entered.get(), 1);
            again.release();
            host.close().await
        })
        .unwrap();
}
#[test]
fn retained_released_wrapper_is_not_a_live_consumer_during_version_adoption() {
    let (host, driver) = host();
    let old = token(
        "released-owner",
        1,
        "old",
        json!({}),
        json!({"type":"integer"}),
    );
    let next = token(
        "released-owner",
        2,
        "new",
        json!({}),
        json!({"type":"integer"}),
    );
    let options = ServeOptions {
        lifetime: Lifetime::Host,
        ..Default::default()
    };
    driver
        .run_until(async {
            host.install_executable(
                serve(
                    "released",
                    old.clone(),
                    "old",
                    1,
                    options.clone(),
                    Rc::new(Cell::new(0)),
                ),
                LoadOptions::default(),
            )?
            .await?;
            let released = host.get(old, json!({}), GetOptions::default()).await?;
            let id = released.object_id();
            released.release();
            assert_eq!(released.status(), RefStatus::Released);
            let report = host
                .reload(
                    vec![serve(
                        "released",
                        next.clone(),
                        "new",
                        2,
                        options,
                        Rc::new(Cell::new(0)),
                    )],
                    ReloadOptions::default(),
                )
                .await?;
            assert_eq!(report.current_version(), 2);
            assert_eq!(released.status(), RefStatus::Released);
            let current = host.get(next, json!({}), GetOptions::default()).await?;
            assert_eq!(current.object_id(), id);
            assert_eq!(current.call_json("new", None, None).await?, Some(json!(2)));
            assert_eq!(
                released
                    .call_json("old", None, None)
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::Released
            );
            current.release();
            host.close().await
        })
        .unwrap();
}

#[test]
fn lazy_provider_contract_is_checked_even_without_a_live_object() {
    let (host, driver) = host();
    let old = token("lazy-owner", 1, "old", json!({}), json!({"type":"integer"}));
    let candidate = token("lazy-owner", 1, "new", json!({}), json!({"type":"integer"}));
    driver
        .run_until(async {
            host.install_executable(
                serve(
                    "lazy",
                    old,
                    "old",
                    1,
                    ServeOptions::default(),
                    Rc::new(Cell::new(0)),
                ),
                LoadOptions::default(),
            )?
            .await?;
            let error = host
                .reload(
                    vec![serve(
                        "lazy",
                        candidate,
                        "new",
                        2,
                        ServeOptions::default(),
                        Rc::new(Cell::new(0)),
                    )],
                    ReloadOptions::default(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ContractMismatch);
            host.close().await
        })
        .unwrap();
}

#[test]
fn surviving_facet_uses_requires_compatibility_even_without_a_declared_reference() {
    let (host, driver) = host();
    let old = token("used-owner", 1, "old", json!({}), json!({"type":"integer"}));
    let candidate = token("used-owner", 2, "new", json!({}), json!({"type":"integer"}));
    let consumer =
        ExecutableFacet::foreign("unchanged-use", vec![old.clone()], |_, _| Ok(())).unwrap();
    driver
        .run_until(async {
            host.load_executable(
                vec![
                    serve(
                        "used-provider",
                        old,
                        "old",
                        1,
                        ServeOptions::default(),
                        Rc::new(Cell::new(0)),
                    ),
                    consumer,
                ],
                LoadOptions::default(),
            )?
            .await?;
            let error = host
                .reload(
                    vec![serve(
                        "used-provider",
                        candidate,
                        "new",
                        2,
                        ServeOptions::default(),
                        Rc::new(Cell::new(0)),
                    )],
                    ReloadOptions::default(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ContractMismatch);
            host.close().await
        })
        .unwrap();
}
#[test]
fn updated_consumers_bind_the_pending_provider_batch_and_old_context_keeps_its_old_dependency() {
    let (host, driver) = host();
    let old = token(
        "pending-provider",
        1,
        "old",
        json!({}),
        json!({"type":"integer"}),
    );
    let current = token(
        "pending-provider",
        2,
        "new",
        json!({}),
        json!({"type":"integer"}),
    );
    let consumer = token(
        "pending-consumer",
        1,
        "read",
        json!({}),
        json!({"type":"integer"}),
    );
    let consumer_facet = |provider: Rc<Contract>, member: &str| {
        let consumer = consumer.clone();
        let member = member.to_owned();
        ExecutableFacet::foreign(
            "pending-consumer-facet",
            vec![provider.clone()],
            move |_, setup| {
                let provider = provider.clone();
                let member = member.clone();
                setup.serve(
                    consumer.clone(),
                    Rc::new(move |_, _, declaration| {
                        let dependency =
                            declaration.get(provider.clone(), json!({}), GetOptions::default())?;
                        let member = member.clone();
                        Ok(
                            Implementation::new().json_method("read", move |_, context| {
                                let dependency = dependency.clone();
                                let member = member.clone();
                                async move {
                                dependency.call_json(&member, None, Some(&context)).await
                            }
                            .boxed_local()
                            }),
                        )
                    }),
                    ServeOptions {
                        lifetime: Lifetime::Host,
                        ..Default::default()
                    },
                )
            },
        )
        .unwrap()
    };
    driver
        .run_until(async {
            host.load_executable(
                vec![
                    serve(
                        "pending-provider-facet",
                        old.clone(),
                        "old",
                        1,
                        ServeOptions::default(),
                        Rc::new(Cell::new(0)),
                    ),
                    consumer_facet(old, "old"),
                ],
                LoadOptions::default(),
            )?
            .await?;
            let reference = host
                .get(consumer.clone(), json!({}), GetOptions::default())
                .await?;
            let definitions = vec![
                consumer_facet(current.clone(), "new"),
                serve(
                    "pending-provider-facet",
                    current,
                    "new",
                    2,
                    ServeOptions::default(),
                    Rc::new(Cell::new(0)),
                ),
            ];
            let reload_host = host.clone();
            host.root(move |old_context| {
                async move {
                    assert_eq!(
                        reference
                            .call_json("read", None, Some(&old_context))
                            .await?,
                        Some(json!(1))
                    );
                    reload_host
                        .reload(definitions, ReloadOptions::default())
                        .await?;
                    assert_eq!(
                        reference.call_json("read", None, None).await?,
                        Some(json!(2))
                    );
                    assert_eq!(
                        reference
                            .call_json("read", None, Some(&old_context))
                            .await?,
                        Some(json!(1))
                    );
                    reference.release();
                    Ok(())
                }
                .boxed_local()
            })
            .await?;
            host.close().await
        })
        .unwrap();
}

fn reject_unowned_takeover(origin: &str) {
    let (host, driver) = host();
    let contract = token(
        "owned-service",
        1,
        "read",
        json!({}),
        json!({"type":"integer"}),
    );
    let owner = contract.clone();
    let factory: Factory = Rc::new(|_, _, _| {
        Ok(Implementation::new()
            .json_method("read", |_, _| async { Ok(Some(json!(1))) }.boxed_local()))
    });
    match origin {
        "direct" => host
            .serve(contract.clone(), factory, ServeOptions::default())
            .unwrap(),
        "native" => {
            let factory = factory.clone();
            let facet = NativeFacet::new("original-owner", vec![], move |_, setup| {
                setup.serve(owner.clone(), factory.clone(), ServeOptions::default())
            })
            .unwrap();
            driver
                .run_until(host.install(facet, LoadOptions::default()).unwrap())
                .unwrap();
        }
        "foreign" => {
            let facet = serve(
                "original-owner",
                owner,
                "read",
                1,
                ServeOptions::default(),
                Rc::new(Cell::new(0)),
            );
            driver
                .run_until(
                    host.install_executable(facet, LoadOptions::default())
                        .unwrap(),
                )
                .unwrap();
        }
        _ => unreachable!(),
    }
    driver
        .run_until(async {
            let reference = host
                .get(contract.clone(), json!({}), GetOptions::default())
                .await?;
            let id = reference.object_id();
            let candidate_calls = Rc::new(Cell::new(0));
            let candidate = serve(
                "unrelated-candidate",
                contract.clone(),
                "read",
                2,
                ServeOptions::default(),
                candidate_calls.clone(),
            );
            let error = host
                .reload(vec![candidate], ReloadOptions::default())
                .await
                .unwrap_err();
            assert_eq!(error.reason, "duplicate", "{origin}");
            assert_eq!(candidate_calls.get(), 0, "{origin}");
            assert_eq!(
                reference.call_json("read", None, None).await?,
                Some(json!(1))
            );
            let fresh = host.get(contract, json!({}), GetOptions::default()).await?;
            assert_eq!(fresh.object_id(), id);
            assert_eq!(fresh.call_json("read", None, None).await?, Some(json!(1)));
            assert_eq!(
                host.root(|ctx| async move { Ok(ctx.code_version()) }.boxed_local())
                    .await?,
                1
            );
            fresh.release();
            reference.release();
            host.close().await
        })
        .unwrap();
}

#[test]
fn a_candidate_cannot_take_over_a_direct_service() {
    reject_unowned_takeover("direct");
}
#[test]
fn a_candidate_cannot_take_over_a_native_service() {
    reject_unowned_takeover("native");
}
#[test]
fn a_candidate_cannot_take_over_an_unreplaced_foreign_service() {
    reject_unowned_takeover("foreign");
}

#[test]
fn a_service_installed_while_preparing_cannot_be_overwritten_by_the_candidate() {
    use futures::{
        channel::oneshot,
        future::{Either, select},
    };
    use std::cell::RefCell;
    let (host, driver) = host();
    let contract = token(
        "late-owned-service",
        1,
        "read",
        json!({}),
        json!({"type":"integer"}),
    );
    let (opened, opening) = oneshot::channel();
    let (finish, finishing) = oneshot::channel();
    let opening_started = Rc::new(RefCell::new(Some(opened)));
    let finishing = Rc::new(RefCell::new(Some(finishing)));
    let candidate_token = contract.clone();
    let closed = Rc::new(Cell::new(0));
    let candidate_closed = closed.clone();
    let candidate = ExecutableFacet::foreign("new-candidate", vec![], move |_, setup| {
        let (started, finishing, closed) = (
            opening_started.clone(),
            finishing.clone(),
            candidate_closed.clone(),
        );
        setup.serve(
            candidate_token.clone(),
            Rc::new(move |_, _, declaration| {
                let (started, finishing, closed) =
                    (started.clone(), finishing.clone(), closed.clone());
                declaration.resource(
                    "new",
                    ResourceDeclaration::new(move |_| {
                        started.borrow_mut().take().unwrap().send(()).unwrap();
                        let finishing = finishing.borrow_mut().take().unwrap();
                        async move {
                            finishing.await.unwrap();
                            Ok(())
                        }
                        .boxed_local()
                    })
                    .with_close(move |_| {
                        closed.set(closed.get() + 1);
                        async { Ok(()) }.boxed_local()
                    }),
                )?;
                Ok(Implementation::new()
                    .json_method("read", |_, _| async { Ok(Some(json!(2))) }.boxed_local()))
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
    })
    .unwrap();
    let reload = host.reload(vec![candidate], ReloadOptions::default());
    let Either::Right((_, reload)) = driver.run_until(select(reload, opening)) else {
        panic!("candidate must await its real resource opener");
    };
    host.serve(
        contract.clone(),
        Rc::new(|_, _, _| {
            Ok(Implementation::new()
                .json_method("read", |_, _| async { Ok(Some(json!(9))) }.boxed_local()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    finish.send(()).unwrap();
    let error = driver.run_until(reload).unwrap_err();
    assert_eq!(error.reason, "duplicate");
    assert_eq!(closed.get(), 1);
    driver
        .run_until(async {
            let current = host.get(contract, json!({}), GetOptions::default()).await?;
            assert_eq!(current.call_json("read", None, None).await?, Some(json!(9)));
            assert_eq!(
                host.root(|ctx| async move { Ok(ctx.code_version()) }.boxed_local())
                    .await?,
                1
            );
            current.release();
            host.close().await
        })
        .unwrap();
}
