use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{
    contracts::{Contract, Member, Placement},
    rpc::*,
    *,
};

fn local(name: &str) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), BTreeMap::new()).unwrap())
}

fn eager() -> ServeOptions {
    ServeOptions {
        lifetime: Lifetime::Host,
        ..Default::default()
    }
}

fn host(name: &str, driver: &Rc<ManualDriver>) -> Host {
    Host::new(
        HostOptions::new(name, json!("host-principal")),
        driver.clone(),
    )
}

#[test]
fn definitions_preserve_identity_and_independent_consumed_tokens_without_setup() {
    let first = local("uses");
    let second = Rc::new(Contract::local("uses", 2, json!({}), BTreeMap::new()).unwrap());
    let mut source = vec![first.clone(), second.clone()];
    let setups = Rc::new(Cell::new(0));
    let calls = setups.clone();
    let facet = NativeFacet::new(" exact name ", source.clone(), move |_, _| {
        calls.set(calls.get() + 1);
        Ok(())
    })
    .unwrap();
    source.clear();
    assert_eq!(facet.name(), " exact name ");
    assert_eq!(facet.uses().len(), 2);
    assert!(Rc::ptr_eq(&facet.uses()[0], &first));
    assert!(Rc::ptr_eq(&facet.clone().uses()[1], &second));
    assert_eq!(setups.get(), 0);
    let driver = ManualDriver::new();
    let host = host("uses", &driver);
    driver
        .run_until(host.install(facet, LoadOptions::default()).unwrap())
        .unwrap();
    assert_eq!(
        setups.get(),
        1,
        "uses does not acquire its unserved providers"
    );
    driver.run_until(host.close()).unwrap();
    for name in ["", "bad\nname", "bad\0name", "bad\u{7f}name"] {
        let error = NativeFacet::new(name, vec![], |_, _| Ok(())).err().unwrap();
        assert_eq!(error.reason, "facet_name");
    }
}

#[test]
fn setup_is_synchronous_and_dropped_startup_does_not_release_eager_objects() {
    let driver = ManualDriver::new();
    let host = host("synchronous", &driver);
    let setups = Rc::new(Cell::new(0));
    let opens = Rc::new(Cell::new(0));
    let closes = Rc::new(Cell::new(0));
    let token = local("synchronous-object");
    let observed = setups.clone();
    let factory_opens = opens.clone();
    let factory_closes = closes.clone();
    let definition = NativeFacet::new("synchronous", vec![], move |scope, declaration| {
        assert_eq!(scope.host_name(), "synchronous");
        assert!(!scope.signal().is_cancelled());
        observed.set(observed.get() + 1);
        let opens = factory_opens.clone();
        let closes = factory_closes.clone();
        declaration.serve(
            token.clone(),
            Rc::new(move |_, _, declaration| {
                let opens = opens.clone();
                let closes = closes.clone();
                declaration.resource(
                    "owned",
                    ResourceDeclaration::new(move |_| {
                        opens.set(opens.get() + 1);
                        async { Ok(()) }.boxed_local()
                    })
                    .with_close(move |_| {
                        closes.set(closes.get() + 1);
                        async { Ok(()) }.boxed_local()
                    }),
                )?;
                Ok(Implementation::new())
            }),
            eager(),
        )
    })
    .unwrap();
    let duplicate = definition.clone();
    let startup = host.install(definition, LoadOptions::default()).unwrap();
    assert_eq!(setups.get(), 1, "setup ran before polling startup");
    assert!(host.install(duplicate, LoadOptions::default()).is_err());
    assert_eq!(setups.get(), 1, "duplicate refusal did not run setup");
    drop(startup);
    driver.run_until(host.ready()).unwrap();
    assert_eq!(opens.get(), 1);
    assert_eq!(closes.get(), 0, "the Host pin outlives the dropped waiter");
    driver.run_until(host.close()).unwrap();
    assert_eq!(closes.get(), 1);
}

#[test]
fn every_setup_in_a_batch_precedes_factories_and_later_dependencies_work() {
    let driver = ManualDriver::new();
    let host = host("batch", &driver);
    let first = local("batch-first");
    let second = local("batch-second");
    let declarations = Rc::new(Cell::new(0));
    let factories = Rc::new(Cell::new(0));
    let declared_first = declarations.clone();
    let first_factories = factories.clone();
    let dependency = second.clone();
    let a = NativeFacet::new("a", vec![second.clone()], move |_, declaration| {
        declared_first.set(declared_first.get() + 1);
        let declared = declared_first.clone();
        let factories = first_factories.clone();
        let dependency = dependency.clone();
        declaration.serve(
            first.clone(),
            Rc::new(move |_, _, declaration| {
                assert_eq!(declared.get(), 2, "all setup bodies preceded the factory");
                factories.set(factories.get() + 1);
                declaration.get(dependency.clone(), json!({}), GetOptions::default())?;
                Ok(Implementation::new())
            }),
            eager(),
        )
    })
    .unwrap();
    let declared_second = declarations.clone();
    let b = NativeFacet::new("b", vec![], move |_, declaration| {
        declared_second.set(declared_second.get() + 1);
        declaration.serve(
            second.clone(),
            Rc::new(|_, _, _| Ok(Implementation::new())),
            ServeOptions::default(),
        )
    })
    .unwrap();
    let startup = host.load(vec![a, b], LoadOptions::default()).unwrap();
    assert_eq!(declarations.get(), 2);
    driver.run_until(startup).unwrap();
    assert_eq!(factories.get(), 1);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn deferred_startup_keeps_physical_open_and_snapshot_ready_waits_for_it() {
    let driver = ManualDriver::new();
    let host = host("deferred", &driver);
    let token = local("deferred-object");
    let (complete, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    let opened = Rc::new(Cell::new(false));
    let observed = opened.clone();
    let definition = NativeFacet::new("deferred", vec![], move |_, declaration| {
        let receive = receive.clone();
        let opened = observed.clone();
        declaration.serve(
            token.clone(),
            Rc::new(move |_, _, declaration| {
                let receive = receive.borrow_mut().take().unwrap();
                let opened = opened.clone();
                let receive = Rc::new(RefCell::new(Some(receive)));
                declaration.resource(
                    "gate",
                    ResourceDeclaration::new(move |_| {
                        let receive = receive.borrow_mut().take().unwrap();
                        let opened = opened.clone();
                        async move {
                            receive.await.unwrap();
                            opened.set(true);
                            Ok(())
                        }
                        .boxed_local()
                    }),
                )?;
                Ok(Implementation::new())
            }),
            eager(),
        )
    })
    .unwrap();
    driver
        .run_until(
            host.install(
                definition,
                LoadOptions {
                    await_host_objects: false,
                },
            )
            .unwrap(),
        )
        .unwrap();
    driver.run_until_stalled();
    assert!(!opened.get());
    assert!(host.ready().now_or_never().is_none());
    complete.send(()).unwrap();
    driver.run_until(host.ready()).unwrap();
    assert!(opened.get());
    driver.run_until(host.close()).unwrap();
}

#[test]
fn failed_scoped_setup_withdraws_only_its_services_and_skips_later_facets() {
    let driver = ManualDriver::new();
    let host = host("failure", &driver);
    let installed = local("failure-success");
    let failed_one = local("failure-withdrawn-one");
    let failed_two = local("failure-withdrawn-two");
    let external = local("failure-direct-host-effect");
    let successful_factories = Rc::new(Cell::new(0));
    let started = successful_factories.clone();
    let successful_token = installed.clone();
    let a = NativeFacet::new("success", vec![], move |_, declaration| {
        let started = started.clone();
        declaration.serve(
            successful_token.clone(),
            Rc::new(move |_, _, _| {
                started.set(started.get() + 1);
                Ok(Implementation::new())
            }),
            eager(),
        )
    })
    .unwrap();
    let failure_signal = Rc::new(RefCell::new(None));
    let signal = failure_signal.clone();
    let first = failed_one.clone();
    let second = failed_two.clone();
    let direct_host = host.clone();
    let direct_token = external.clone();
    let mut cause = RuntimeError::new(ErrorCode::Forbidden, "setup_policy", "setup refused");
    cause.set_details(json!({"original":true}));
    let original = cause.clone();
    let b = NativeFacet::new("failure", vec![], move |scope, declaration| {
        *signal.borrow_mut() = Some(scope.signal());
        for token in [&first, &second] {
            declaration.serve(
                token.clone(),
                Rc::new(|_, _, _| panic!("withdrawn factory must never run")),
                eager(),
            )?;
        }
        direct_host.serve(
            direct_token.clone(),
            Rc::new(|_, _, _| Ok(Implementation::new())),
            ServeOptions::default(),
        )?;
        Err(cause.clone())
    })
    .unwrap();
    let later_called = Rc::new(Cell::new(false));
    let later = later_called.clone();
    let c = NativeFacet::new("later", vec![], move |_, _| {
        later.set(true);
        Ok(())
    })
    .unwrap();
    let error = host
        .load(vec![a, b, c], LoadOptions::default())
        .err()
        .unwrap();
    assert_eq!(error, original);
    assert!(failure_signal.borrow().as_ref().unwrap().is_cancelled());
    assert!(!later_called.get());
    for token in [failed_one, failed_two] {
        assert_eq!(
            host.hold(token, json!({}), GetOptions::default())
                .err()
                .unwrap()
                .reason,
            "unknown_type"
        );
    }
    driver.run_until(host.ready()).unwrap();
    assert_eq!(successful_factories.get(), 1);
    let success = host
        .hold(installed, json!({}), GetOptions::default())
        .unwrap();
    let external = host
        .hold(external, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until(external.ready()).unwrap();
    assert_eq!(success.status(), RefStatus::Live);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn readiness_is_captured_on_call_and_fails_without_waiting_for_a_pending_peer() {
    let driver = ManualDriver::new();
    let host = host("readiness", &driver);
    let before = host.ready();
    let (complete, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    host.serve(
        local("readiness-pending"),
        Rc::new(move |_, _, declaration| {
            let receive = receive.clone();
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
    let mut cause = RuntimeError::new(ErrorCode::Forbidden, "startup_policy", "factory refused");
    cause.set_details(json!({"source":"factory"}));
    let original = cause.clone();
    host.serve(
        local("readiness-failed"),
        Rc::new(move |_, _, _| Err(cause.clone())),
        eager(),
    )
    .unwrap();
    driver.run_until(before).unwrap();
    let error = driver.run_until(host.ready()).unwrap_err();
    assert_eq!(error, original);
    driver.run_until_stalled();
    assert_eq!(driver.run_until(host.ready()).unwrap_err(), original);
    assert_eq!(host.errors(), vec![original]);
    complete.send(()).unwrap();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn retained_scope_is_weak_and_observes_close_without_owning_setup_captures() {
    let driver = ManualDriver::new();
    let host = host("weak", &driver);
    let saved = Rc::new(RefCell::new(None::<FacetScope>));
    let capture = Rc::new(());
    let weak_capture = Rc::downgrade(&capture);
    let retained = saved.clone();
    let definition = NativeFacet::new("weak", vec![], move |scope, _| {
        let _keep_capture = &capture;
        *retained.borrow_mut() = Some(scope.clone());
        Ok(())
    })
    .unwrap();
    driver
        .run_until(host.install(definition, LoadOptions::default()).unwrap())
        .unwrap();
    let scope = saved.borrow_mut().take().unwrap();
    let signal = scope.signal();
    assert_eq!(scope.host_name(), "weak");
    assert!(!signal.is_cancelled());
    let registry = Registry::new("facet-view", ConflictPolicy::Error).unwrap();
    let view = scope.registry(&registry).unwrap();
    assert!(view.table(None).unwrap().is_empty());
    assert_eq!(view.table(None).unwrap().code_version(), 1);
    driver
        .run_until(scope.root(|ctx| async move { Ok(ctx.code_version()) }.boxed_local()))
        .unwrap();
    driver.run_until(host.close()).unwrap();
    assert!(signal.is_cancelled());
    assert!(view.table(None).is_err());
    assert!(scope.registry(&registry).is_err());
    assert!(
        weak_capture.upgrade().is_none(),
        "explicit close drops setup owners"
    );
    assert_eq!(
        driver
            .run_until(scope.root(|_| async { Ok(()) }.boxed_local()))
            .unwrap_err()
            .reason,
        "closed"
    );
    drop(host);
    assert_eq!(
        driver
            .run_until(scope.root(|_| async { Ok(()) }.boxed_local()))
            .unwrap_err()
            .reason,
        "closed"
    );
}

#[test]
fn canceled_facet_signal_does_not_block_cleanup_root_over_a_held_rpc_dependency() {
    let driver = ManualDriver::new();
    let client = host("facet-client", &driver);
    let server = host("facet-server", &driver);
    for (host, nonce) in [
        (&client, "11111111111111111111111111111111"),
        (&server, "22222222222222222222222222222222"),
    ] {
        host.set_rpc_identity(RootOrigin::new(host.name(), nonce).unwrap())
            .unwrap();
    }
    let dependency = Rc::new(
        Contract::remote(
            "facet-flush",
            1,
            json!({}),
            Placement::Named {
                name: server.name().into(),
            },
            BTreeMap::from([(
                "default".into(),
                BTreeMap::from([(
                    "flush".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                )]),
            )]),
        )
        .unwrap(),
    );
    let flushes = Rc::new(Cell::new(0));
    let observed = flushes.clone();
    server
        .serve(
            dependency.clone(),
            Rc::new(move |_, _, _| {
                let observed = observed.clone();
                Ok(Implementation::new()
                    .variant("default")
                    .json_method("flush", move |_, ctx| {
                        assert_eq!(ctx.code_version(), 1);
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
            server.name(),
            json!("parent"),
            ConnectionOptions::default(),
        )
        .unwrap();
    let inbound = server
        .accept_child(
            right,
            client.name(),
            json!("child"),
            ConnectionOptions::default(),
        )
        .unwrap();
    client.set_route(server.name(), outbound.clone()).unwrap();
    let cleaned = Rc::new(Cell::new(false));
    let cleanup = cleaned.clone();
    let definition = NativeFacet::new(
        "cleanup",
        vec![dependency.clone()],
        move |scope, declaration| {
            let scope = scope.clone();
            let dependency = dependency.clone();
            let cleaned = cleanup.clone();
            declaration.serve(
                local("facet-cleanup-owner"),
                Rc::new(move |_, _, declaration| {
                    let dependency =
                        declaration.get(dependency.clone(), json!({}), GetOptions::default())?;
                    let held = dependency.clone();
                    let scope = scope.clone();
                    let cleaned = cleaned.clone();
                    let mut resource = ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local())
                        .with_close(move |_| {
                            assert!(
                                scope.signal().is_cancelled(),
                                "Host closes the run before resource cleanup"
                            );
                            let held = held.clone();
                            let cleaned = cleaned.clone();
                            scope.root(move |ctx| {
                                async move {
                                    held.call_json("flush", None, Some(&ctx)).await?;
                                    cleaned.set(true);
                                    Ok(())
                                }
                                .boxed_local()
                            })
                        });
                    resource
                        .deps
                        .push(ResourceDependency::Reference(dependency));
                    declaration.resource("buffer", resource)?;
                    Ok(Implementation::new())
                }),
                eager(),
            )
        },
    )
    .unwrap();
    driver
        .run_until(client.install(definition, LoadOptions::default()).unwrap())
        .unwrap();
    assert_eq!(outbound.status(), ConnectionStatus::Open);
    driver.run_until(client.close()).unwrap();
    assert!(cleaned.get());
    assert_eq!(flushes.get(), 1);
    assert_eq!(outbound.status(), ConnectionStatus::Closed);
    driver.run_until_stalled();
    assert_eq!(inbound.status(), ConnectionStatus::Closed);
    driver.run_until(server.close()).unwrap();
}

#[test]
fn retained_scope_does_not_keep_an_empty_host_or_its_definition_alive() {
    let driver = ManualDriver::new();
    let host = host("dropped", &driver);
    let retained = Rc::new(RefCell::new(None::<FacetScope>));
    let saved = retained.clone();
    let capture = Rc::new(());
    let weak_capture = Rc::downgrade(&capture);
    let definition = NativeFacet::new("dropped", vec![], move |scope, _| {
        let _keep_capture = &capture;
        *saved.borrow_mut() = Some(scope.clone());
        Ok(())
    })
    .unwrap();
    driver
        .run_until(host.install(definition, LoadOptions::default()).unwrap())
        .unwrap();
    let scope = retained.borrow_mut().take().unwrap();
    let unpolled_root = scope.root(|_| async { Ok(()) }.boxed_local());
    drop(host);
    assert!(weak_capture.upgrade().is_none());
    assert_eq!(
        driver.run_until(unpolled_root).unwrap_err().reason,
        "closed"
    );
}

#[test]
fn native_setup_unwind_restores_declaration_admission_without_rolling_back_prior_facets() {
    let driver = ManualDriver::new();
    let host = host("unwind", &driver);
    let started = Rc::new(Cell::new(0));
    let observed = started.clone();
    let first = NativeFacet::new("before-unwind", vec![], move |_, declaration| {
        let observed = observed.clone();
        declaration.serve(
            local("before-unwind-object"),
            Rc::new(move |_, _, _| {
                observed.set(observed.get() + 1);
                Ok(Implementation::new())
            }),
            eager(),
        )
    })
    .unwrap();
    let failed_signal = Rc::new(RefCell::new(None));
    let signal = failed_signal.clone();
    let failed_token = local("unwound-object");
    let partial = failed_token.clone();
    let second = NativeFacet::new("unwind", vec![], move |scope, declaration| {
        *signal.borrow_mut() = Some(scope.signal());
        declaration.serve(
            partial.clone(),
            Rc::new(|_, _, _| panic!("withdrawn factory must not run")),
            eager(),
        )?;
        panic!("native setup panic propagates");
    })
    .unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _startup = host.load(vec![first, second], LoadOptions::default());
    }));
    assert!(panic.is_err());
    assert!(failed_signal.borrow().as_ref().unwrap().is_cancelled());
    assert_eq!(
        host.hold(failed_token, json!({}), GetOptions::default())
            .err()
            .unwrap()
            .reason,
        "unknown_type"
    );
    // No user activation is run during unwind. The next successful declaration release
    // schedules the surviving earlier facet through the ordinary Host owner.
    let after = NativeFacet::new("after-unwind", vec![], |_, _| Ok(())).unwrap();
    driver
        .run_until(host.install(after, LoadOptions::default()).unwrap())
        .unwrap();
    driver.run_until(host.ready()).unwrap();
    assert_eq!(started.get(), 1);
    driver.run_until(host.close()).unwrap();
}
