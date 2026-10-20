use super::*;
use crate::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, rc::Rc};

#[test]
fn accepted_parent_decision_then_real_graph_failure_is_terminal_and_still_closes_resources() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("decision", json!(null)), driver.clone());
    let token = Rc::new(
        Contract::local(
            "decision-owner",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Native("decision.value@1".into())),
                },
            )]),
        )
        .unwrap(),
    );
    let closed = Rc::new(Cell::new(0));
    let called = Rc::new(Cell::new(0));
    let definition = |value: u64| {
        let token = token.clone();
        let closed = closed.clone();
        let called = called.clone();
        ExecutableFacet::foreign("decision", Vec::new(), move |_, setup| {
            let closed = closed.clone();
            let called = called.clone();
            setup.serve(
                token.clone(),
                Rc::new(move |_, _, declaration| {
                    let closed = closed.clone();
                    declaration.resource(
                        "resource",
                        ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                            move |_| {
                                closed.set(closed.get() + 1);
                                async { Ok(()) }.boxed_local()
                            },
                        ),
                    )?;
                    let called = called.clone();
                    Ok(Implementation::new().method("read", move |(): (), _| {
                        called.set(called.get() + 1);
                        async move { Ok(value) }.boxed_local()
                    }))
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
        })
        .unwrap()
    };
    driver
        .run_until(async {
            host.install_executable(definition(1), LoadOptions::default())?
                .await?;
            let reference = host
                .get(token.clone(), json!({}), GetOptions::default())
                .await?;
            let mut ready = host
                .inner
                .prepare_reload_at(vec![definition(2)], ReloadOptions::default(), None)
                .await?;
            ready.accept_parent_decision()?;
            let object = reference.claim.local_object().unwrap();
            object.state.terminate(RuntimeError::invalid(
                "participant_failed",
                "actual prepared graph terminated after decision",
            ));
            let error = ready.adopt().unwrap_err();
            assert_eq!(error.reason, "host_failed");
            drop(ready);
            assert_eq!(
                reference
                    .call::<(), u64>("read", (), None)
                    .await
                    .unwrap_err()
                    .reason,
                "host_failed"
            );
            assert_eq!(called.get(), 0);
            assert_eq!(
                host.hold(token.clone(), json!({}), GetOptions::default())
                    .err()
                    .unwrap()
                    .reason,
                "host_failed"
            );
            assert_eq!(
                host.root(|_| async { Ok(()) }.boxed_local())
                    .await
                    .unwrap_err()
                    .reason,
                "host_failed"
            );
            host.close().await?;
            assert_eq!(
                closed.get(),
                1,
                "native cleanup still runs after managed admission failed"
            );
            Ok::<_, RuntimeError>(())
        })
        .unwrap();
}

#[test]
fn unrelated_activation_waits_then_publishes_current_without_relabelling_initializer() {
    use futures::channel::oneshot;
    use std::cell::RefCell;
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("publication", json!(null)), driver.clone());
    let registry = Registry::new("unrelated", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let token = Rc::new(
        Contract::local(
            "unrelated-owner",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Native("unrelated.epoch@1".into())),
                },
            )]),
        )
        .unwrap(),
    );
    let (enter, entered) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let enter = Rc::new(RefCell::new(Some(enter)));
    let released = Rc::new(RefCell::new(Some(released)));
    let initialized_version = Rc::new(Cell::new(0));
    let initialized = initialized_version.clone();
    host.serve(
        token.clone(),
        Rc::new(move |_, _, declaration| {
            declaration.register(&registry, "entry", json!({"value":1}))?;
            let enter = enter.clone();
            let released = released.clone();
            let initialized = initialized.clone();
            declaration.state(
                "waiting",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |context| {
                    let enter = enter.borrow_mut().take().unwrap();
                    let released = released.borrow_mut().take().unwrap();
                    let initialized = initialized.clone();
                    async move {
                        initialized.set(context.code_version());
                        let _ = enter.send(());
                        let _ = released.await;
                        Ok(json!(1))
                    }
                    .boxed_local()
                }),
            )?;
            Ok(Implementation::new().method("read", |(): (), context| {
                async move { Ok(context.code_version()) }.boxed_local()
            }))
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    let reference = host.hold(token, json!({}), GetOptions::default()).unwrap();
    let entered = driver.run_until(futures::future::select(
        entered,
        reference.ready().boxed_local(),
    ));
    match entered {
        futures::future::Either::Left((result, _)) => result.unwrap(),
        futures::future::Either::Right((result, _)) => {
            panic!("initializer did not enter: {result:?}")
        }
    };
    let selected = reference.clone();
    let inner = host.inner.clone();
    driver
        .run_until(host.root(move |old_context| {
            async move {
                let mut ready = inner
                    .prepare_reload_at(Vec::new(), ReloadOptions::default(), None)
                    .await?;
                assert!(inner.reload_ready.get());
                release.send(()).unwrap();
                inner.driver.sleep(std::time::Duration::ZERO).await;
                assert_eq!(selected.status(), RefStatus::Loading);
                assert!(view.table(None)?.get("entry").is_none());
                ready.accept_parent_decision()?;
                let report = ready.adopt()?;
                ready.finish(report).await?;
                selected.ready().await?;
                assert!(view.table(None)?.get("entry").is_some());
                assert_eq!(initialized_version.get(), 1);
                assert_eq!(selected.call::<(), u64>("read", (), None).await?, 2);
                assert_eq!(
                    selected
                        .call::<(), u64>("read", (), Some(&old_context))
                        .await
                        .unwrap_err()
                        .code,
                    ErrorCode::Superseded
                );
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    reference.release();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn ready_refuses_a_late_unchecked_consumer_but_allows_prevalidated_profiles() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("ready-binding", json!(null)),
        driver.clone(),
    );
    let token = |version| {
        Rc::new(
            Contract::local(
                "ready-provider",
                version,
                json!({}),
                BTreeMap::from([(
                    "read".into(),
                    Member::Method {
                        args: None,
                        result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                    },
                )]),
            )
            .unwrap(),
        )
    };
    let (legacy, original, next) = (token(1), token(2), token(3));
    let late_adapter_calls = Rc::new(Cell::new(0));
    let candidate_adapter_calls = Rc::new(Cell::new(0));
    let definition = |contract: Rc<Contract>, value: i64, options: ServeOptions| {
        ExecutableFacet::foreign("ready-provider-facet", vec![], move |_, setup| {
            setup.serve(
                contract.clone(),
                Rc::new(move |_, _, _| {
                    Ok(Implementation::new().json_method("read", move |_, _| {
                        async move { Ok(Some(json!(value))) }.boxed_local()
                    }))
                }),
                options.clone(),
            )
        })
        .unwrap()
    };
    let late_count = late_adapter_calls.clone();
    let old_options = ServeOptions {
        lifetime: Lifetime::Host,
        ..Default::default()
    }
    .with_adapter(
        1,
        Rc::new(move |implementation| {
            late_count.set(late_count.get() + 1);
            Ok(implementation.as_ref().clone())
        }),
    );
    let candidate_count = candidate_adapter_calls.clone();
    let next_options = ServeOptions {
        lifetime: Lifetime::Host,
        ..Default::default()
    }
    .with_adapter(
        2,
        Rc::new(move |implementation| {
            candidate_count.set(candidate_count.get() + 1);
            Ok(implementation.as_ref().clone())
        }),
    );
    driver
        .run_until(async {
            host.install_executable(
                definition(original.clone(), 2, old_options),
                LoadOptions::default(),
            )?
            .await?;
            let original_claim = host
                .get(original.clone(), json!({}), GetOptions::default())
                .await?;
            let mut prepared = host
                .inner
                .prepare_reload_at(
                    vec![definition(next, 3, next_options)],
                    ReloadOptions::default(),
                    None,
                )
                .await?;
            assert_eq!(candidate_adapter_calls.get(), 1);
            let refused = host.hold(legacy, json!({}), GetOptions::default());
            let refused = refused.err().expect("late unchecked profile must refuse");
            assert_eq!(refused.code, ErrorCode::UpgradeConflict);
            assert!(refused.retryable());
            assert_eq!(refused.reason, "upgrade_conflict");
            assert_eq!(late_adapter_calls.get(), 0);
            original_claim.release();
            let known = host.hold(original, json!({}), GetOptions::default())?;
            prepared.decide()?;
            let report = prepared.adopt()?;
            prepared.finish(report).await?;
            assert_eq!(
                candidate_adapter_calls.get(),
                1,
                "cached view cannot be rebuilt inside adoption"
            );
            assert_eq!(known.call_json("read", None, None).await?, Some(json!(3)));
            known.release();
            host.close().await
        })
        .unwrap();
}

#[test]
fn ready_pin_keeps_a_referenced_object_alive_after_the_last_external_claim_releases() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("pin", json!(null)), driver.clone());
    let token = |version| {
        Rc::new(Contract::local("pin-owner", version, json!({}), BTreeMap::new()).unwrap())
    };
    let (old, next) = (token(1), token(2));
    let definition = |contract: Rc<Contract>, options: ServeOptions| {
        ExecutableFacet::foreign("pin-facet", vec![], move |_, setup| {
            setup.serve(
                contract.clone(),
                Rc::new(|_, _, _| Ok(Implementation::new())),
                options.clone(),
            )
        })
        .unwrap()
    };
    driver
        .run_until(
            host.install_executable(
                definition(old.clone(), ServeOptions::default()),
                LoadOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
    let reference = driver
        .run_until(host.get(old, json!({}), GetOptions::default()))
        .unwrap();
    let id = reference.object_id();
    let object = reference.claim.local_object().unwrap().clone();
    let options = ServeOptions::default().with_adapter(
        1,
        Rc::new(|implementation| Ok(implementation.as_ref().clone())),
    );
    let mut ready = driver
        .run_until(host.inner.prepare_reload_at(
            vec![definition(next.clone(), options)],
            ReloadOptions::default(),
            None,
        ))
        .unwrap();
    reference.release();
    driver.run_until_stalled();
    assert!(
        object.phase.get() == crate::object::Phase::Live,
        "Ready's independent pin must keep the selected object alive"
    );
    assert!(
        object
            .claims
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .any(|claim| matches!(
                (Reference { claim }).status(),
                RefStatus::Loading | RefStatus::Live
            ))
    );
    ready.decide().unwrap();
    let report = ready.adopt().unwrap();
    driver.run_until(ready.finish(report)).unwrap();
    let current = driver
        .run_until(host.get(next, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(current.object_id(), id);
    current.release();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn ready_pins_an_unchanged_projection_owner_until_coherent_adoption() {
    use std::cell::RefCell;
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("projection-pin", json!(null)),
        driver.clone(),
    );
    let token = |name| {
        Rc::new(
            Contract::local(
                name,
                1,
                json!({}),
                BTreeMap::from([(
                    "value".into(),
                    Member::State {
                        schema: json!({"type":"integer"}),
                    },
                )]),
            )
            .unwrap(),
        )
    };
    let source_token = token("projection-source");
    let dependent_token = token("projection-dependent");
    let saved = Rc::new(RefCell::new(None::<crate::Cell>));
    let definition = |version| {
        let token = source_token.clone();
        let saved = saved.clone();
        ExecutableFacet::foreign("projection-source", vec![], move |_, setup| {
            let saved = saved.clone();
            setup.serve(
                token.clone(),
                Rc::new(move |_, _, declaration| {
                    let cell = declaration.state(
                        "value",
                        StateDeclaration::new(json!({"type":"integer"}), json!(2))
                            .version(version)
                            .with_migrate(|_, _| Ok(json!(3))),
                    )?;
                    *saved.borrow_mut() = Some(cell.clone());
                    Ok(Implementation::new().state("value", cell.readable()))
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
        })
        .unwrap()
    };
    driver
        .run_until(
            host.install_executable(definition(1), LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    let input = saved.borrow().as_ref().unwrap().readable();
    host.serve(
        dependent_token.clone(),
        Rc::new(move |_, _, declaration| {
            let value =
                declaration.derive(vec![input.clone()], json!({"type":"integer"}), |values| {
                    Ok(json!(values[0].value().as_i64().unwrap() * 10))
                })?;
            Ok(Implementation::new().state("value", value))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let dependent = driver
        .run_until(host.get(dependent_token.clone(), json!({}), GetOptions::default()))
        .unwrap();
    let id = dependent.object_id();
    let object = dependent.claim.local_object().unwrap().clone();
    let mut ready = driver
        .run_until(host.inner.prepare_reload_at(
            vec![definition(2)],
            ReloadOptions::default(),
            None,
        ))
        .unwrap();
    assert!(
        host.inner
            .reload_bindings
            .borrow()
            .as_ref()
            .unwrap()
            .affected
            .contains(&object.id)
    );
    dependent.release();
    driver.run_until_stalled();
    assert!(
        object.phase.get() == crate::object::Phase::Live,
        "projection-only owner remains physically installed after its last public release"
    );
    ready.decide().unwrap();
    let report = ready.adopt().unwrap();
    let current = host
        .hold(dependent_token, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until(ready.finish(report)).unwrap();
    driver.run_until(current.ready()).unwrap();
    assert_eq!(current.object_id(), id);
    assert_eq!(
        current
            .readable("value")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(30)
    );
    current.release();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn closing_refuses_reload_before_retirement_or_setup_can_start() {
    use futures::channel::oneshot;
    use std::cell::RefCell;
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("closing-reload", json!(null)),
        driver.clone(),
    );
    let token = Rc::new(Contract::local("closing-owner", 1, json!({}), BTreeMap::new()).unwrap());
    let (entered, entered_wait) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let entered = Rc::new(RefCell::new(Some(entered)));
    let released = Rc::new(RefCell::new(Some(released)));
    host.serve(
        token,
        Rc::new(move |_, _, declaration| {
            let entered = entered.clone();
            let released = released.clone();
            declaration.resource(
                "held",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        let entered = entered.borrow_mut().take().unwrap();
                        let released = released.borrow_mut().take().unwrap();
                        async move {
                            let _ = entered.send(());
                            let _ = released.await;
                            Ok(())
                        }
                        .boxed_local()
                    },
                ),
            )?;
            Ok(Implementation::new())
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    driver
        .run_until(host.reload(vec![], ReloadOptions::default()))
        .unwrap();
    let setups = Rc::new(Cell::new(0));
    let calls = setups.clone();
    let candidate = ExecutableFacet::foreign("never-set-up", vec![], move |_, _| {
        calls.set(calls.get() + 1);
        Ok(())
    })
    .unwrap();
    driver.run_until(async {
        let close = host.close().boxed_local();
        let close = match futures::future::select(entered_wait, close).await {
            futures::future::Either::Left((entered, close)) => {
                entered.unwrap();
                close
            }
            futures::future::Either::Right(_) => panic!("resource closer must remain pending"),
        };
        let now = host.inner.driver.now();
        assert_eq!(host.inner.epochs.previous().unwrap().version.get(), 1);
        let Err(error) = host
            .inner
            .prepare_reload_at(vec![candidate], ReloadOptions::default(), None)
            .await
        else {
            panic!("closing Host must refuse before preparing");
        };
        assert_eq!(error.reason, "closed");
        assert_eq!(setups.get(), 0);
        assert_eq!(host.inner.epochs.current().version.get(), 2);
        assert_eq!(
            host.inner.epochs.previous().unwrap().version.get(),
            1,
            "refusal must not retire the previous epoch"
        );
        assert_eq!(host.inner.driver.now(), now);
        release.send(()).unwrap();
        close.await.unwrap();
    });
}
