use futures::{FutureExt, channel::oneshot, future::LocalBoxFuture};
use serde_json::json;
use std::{
    cell::{Cell as Counter, RefCell},
    collections::BTreeMap,
    rc::Rc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
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
fn host(driver: &Rc<ManualDriver>) -> Host {
    Host::new(
        HostOptions::new("integration", json!("service")),
        driver.clone(),
    )
}

/// Polls spawn immediately, rather than relying on the embedding's deferred scheduling.
struct EagerDriver {
    inner: Rc<ManualDriver>,
}
impl Driver for EagerDriver {
    fn spawn(&self, mut future: LocalBoxFuture<'static, ()>) {
        let mut cx = TaskContext::from_waker(futures::task::noop_waker_ref());
        if future.as_mut().poll(&mut cx) == Poll::Pending {
            self.inner.spawn(future);
        }
    }
    fn sleep(&self, duration: Duration) -> LocalBoxFuture<'static, ()> {
        self.inner.sleep(duration)
    }
    fn now(&self) -> Duration {
        self.inner.now()
    }
}

#[test]
fn batch_blocks_even_an_eager_driver_and_nested_factory_dependencies() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("eager", json!(null)),
        Rc::new(EagerDriver {
            inner: driver.clone(),
        }),
    );
    let declared = Rc::new(Counter::new(false));
    let consumer = token("eager-consumer");
    let provider = token("eager-provider");
    let expected = provider.clone();
    let all_declared = declared.clone();
    let a = NativeFacet::new("consumer", vec![], move |_, scope| {
        let declared = all_declared.clone();
        let expected = expected.clone();
        scope.serve(
            consumer.clone(),
            Rc::new(move |_, _, declaration| {
                assert!(declared.get(), "no factory before the last setup");
                declaration.get(expected.clone(), json!({}), GetOptions::default())?;
                Ok(Implementation::new())
            }),
            eager(),
        )
    })
    .unwrap();
    let b = NativeFacet::new("provider", vec![], move |_, scope| {
        scope.serve(
            provider.clone(),
            Rc::new(|_, _, declaration| {
                declaration.state(
                    "initial",
                    StateDeclaration::new(json!({"type":"integer"}), json!(1)),
                )?;
                Ok(Implementation::new())
            }),
            ServeOptions::default(),
        )?;
        declared.set(true);
        Ok(())
    })
    .unwrap();
    driver
        .run_until(host.load(vec![a, b], LoadOptions::default()).unwrap())
        .unwrap();
    assert!(host.errors().is_empty());
    driver.run_until(host.close()).unwrap();
}

#[test]
fn failed_setup_withdraws_reentrantly_acquired_ticket_and_its_readiness_selection() {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let failed = token("withdrawn-ticket");
    let saved = Rc::new(RefCell::new(None));
    let retained = saved.clone();
    let direct_host = host.clone();
    let mut cause = RuntimeError::new(ErrorCode::Forbidden, "setup_refused", "setup refused");
    cause.set_details(json!({"preserved":true}));
    let original = cause.clone();
    let facet = NativeFacet::new("withdrawn", vec![], move |_, scope| {
        scope.serve(
            failed.clone(),
            Rc::new(|_, _, _| panic!("withdrawn ticket executed")),
            eager(),
        )?;
        *retained.borrow_mut() =
            Some(direct_host.hold(failed.clone(), json!({}), GetOptions::default())?);
        Err(cause.clone())
    })
    .unwrap();
    assert_eq!(
        host.install(facet, LoadOptions::default()).err().unwrap(),
        original
    );
    let acquired = saved.borrow_mut().take().unwrap();
    assert_eq!(acquired.status(), RefStatus::Gone);
    assert_eq!(acquired.error().unwrap(), original);
    assert_eq!(driver.run_until(acquired.ready()).unwrap_err(), original);
    driver.run_until(host.ready()).unwrap();
    driver.run_until_stalled();
    assert!(
        host.errors().is_empty(),
        "a withdrawn setup did not start an eager activation"
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn caught_setup_panic_can_be_followed_directly_by_close() {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let calls = Rc::new(Counter::new(0));
    let factory_calls = calls.clone();
    let first = NativeFacet::new("prior", vec![], move |_, scope| {
        let calls = factory_calls.clone();
        scope.serve(
            token("prior-object"),
            Rc::new(move |_, _, _| {
                calls.set(calls.get() + 1);
                Ok(Implementation::new())
            }),
            eager(),
        )
    })
    .unwrap();
    let second = NativeFacet::new("panicking", vec![], |_, scope| {
        scope.serve(
            token("panicking-object"),
            Rc::new(|_, _, _| panic!("withdrawn factory")),
            eager(),
        )?;
        panic!("setup panic");
    })
    .unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _startup = host.load(vec![first, second], LoadOptions::default());
        }))
        .is_err()
    );
    driver.run_until(host.close()).unwrap();
    assert_eq!(
        calls.get(),
        0,
        "close cancels retained unsubmitted startup work"
    );
}

#[test]
fn registry_conflict_does_not_publish_loser_initial_state_and_closes_its_resource() {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let registry = Registry::new("atomic-startup", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let winner_registry = registry.clone();
    host.serve(
        token("registry-winner"),
        Rc::new(move |_, _, declaration| {
            declaration.register(&winner_registry, "same", json!("winner"))?;
            Ok(Implementation::new())
        }),
        eager(),
    )
    .unwrap();
    driver.run_until(host.ready()).unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    let _tables = view
        .subscribe(move |snapshot| {
            observed
                .borrow_mut()
                .push(snapshot.get("same").map(|entry| entry.data().clone()));
            Ok(())
        })
        .unwrap();
    let (complete, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    let saved = Rc::new(RefCell::new(None::<Cell>));
    let factory_saved = saved.clone();
    let closed = Rc::new(Counter::new(0));
    let cleanup = closed.clone();
    host.serve(
        token("registry-loser"),
        Rc::new(move |_, _, declaration| {
            declaration.register(&registry, "same", json!("loser"))?;
            let receive = receive.clone();
            let cell = declaration.state(
                "pending",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |_| {
                    let receive = receive.borrow_mut().take().unwrap();
                    async move {
                        receive.await.unwrap();
                        Ok(json!(7))
                    }
                    .boxed_local()
                }),
            )?;
            *factory_saved.borrow_mut() = Some(cell);
            let cleanup = cleanup.clone();
            declaration.resource(
                "opened",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        cleanup.set(cleanup.get() + 1);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            Ok(Implementation::new())
        }),
        eager(),
    )
    .unwrap();
    driver.run_until_stalled();
    let state_events = Rc::new(Counter::new(0));
    let notifications = state_events.clone();
    let cell = saved.borrow_mut().take().unwrap();
    let _state = cell
        .readable()
        .subscribe(move |_| {
            notifications.set(notifications.get() + 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(state_events.get(), 0);
    complete.send(()).unwrap();
    assert_eq!(
        driver.run_until(host.ready()).unwrap_err().reason,
        "registry_conflict"
    );
    driver.run_until_stalled();
    assert_eq!(closed.get(), 1);
    assert_eq!(
        state_events.get(),
        0,
        "initial loser state never entered publication FIFO"
    );
    assert_eq!(*events.borrow(), vec![Some(json!("winner"))]);
    assert_eq!(
        view.table(None).unwrap().get("same").unwrap().data(),
        &json!("winner")
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn registry_and_state_adopt_before_live_callbacks_and_share_reentrant_fifo() {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let registry = Registry::new("coherent", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let owner = Rc::new(
        Contract::local(
            "coherent-owner",
            1,
            json!({}),
            BTreeMap::from([(
                "count".into(),
                tembra_core::contracts::Member::State {
                    schema: json!({"type":"integer"}),
                },
            )]),
        )
        .unwrap(),
    );
    let saved = Rc::new(RefCell::new(None::<Cell>));
    let factory_saved = saved.clone();
    let (complete, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    host.serve(
        owner.clone(),
        Rc::new(move |_, _, declaration| {
            declaration.register(&registry, "entry", json!("visible"))?;
            let receive = receive.clone();
            let cell = declaration.state(
                "count",
                StateDeclaration::with_initializer(json!({"type":"integer"}), move |_| {
                    let receive = receive.borrow_mut().take().unwrap();
                    async move {
                        receive.await.unwrap();
                        Ok(json!(1))
                    }
                    .boxed_local()
                }),
            )?;
            *factory_saved.borrow_mut() = Some(cell.clone());
            Ok(Implementation::new().state("count", cell.readable()))
        }),
        eager(),
    )
    .unwrap();
    let reference = host.hold(owner, json!({}), GetOptions::default()).unwrap();
    driver.run_until_stalled();
    let cell = saved.borrow_mut().take().unwrap();
    let order = Rc::new(RefCell::new(Vec::new()));
    let observer_order = order.clone();
    let observer_view = view.clone();
    let observed_ref = reference.clone();
    let _state = cell
        .readable()
        .subscribe(move |snapshot| {
            assert_eq!(observed_ref.status(), RefStatus::Live);
            assert_eq!(
                observer_view.table(None)?.get("entry").unwrap().data(),
                &json!("visible")
            );
            observer_order
                .borrow_mut()
                .push(format!("state{}", snapshot.value()));
            Ok(())
        })
        .unwrap();
    let observer_order = order.clone();
    let observer_cell = cell.clone();
    let observer_host = host.clone();
    let observed_ref = reference.clone();
    let _tables = view
        .subscribe(move |snapshot| {
            if snapshot.get("entry").is_none() {
                return Ok(());
            }
            assert_eq!(observed_ref.status(), RefStatus::Live);
            assert_eq!(observer_cell.snapshot()?.value(), &json!(1));
            observer_order.borrow_mut().push("registry1".into());
            let cell = observer_cell.clone();
            observer_host
                .root(move |ctx| {
                    async move {
                        cell.replace(&ctx, json!(2), ReplaceOptions::default())?;
                        Ok(())
                    }
                    .boxed_local()
                })
                .now_or_never()
                .unwrap()?;
            Ok(())
        })
        .unwrap();
    complete.send(()).unwrap();
    driver.run_until(host.ready()).unwrap();
    assert_eq!(*order.borrow(), vec!["registry1", "state1", "state2"]);
    assert_eq!(
        reference
            .readable("count")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(2)
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn deferred_factory_dependency_and_resource_failures_keep_original_cause_once() {
    for kind in ["factory", "dependency", "resource"] {
        let driver = ManualDriver::new();
        let host = host(&driver);
        let mut cause =
            RuntimeError::new(ErrorCode::Forbidden, "startup_policy", "startup refused");
        cause.set_details(json!({"origin":kind}));
        let original = cause.clone();
        let dependency = token("failure-dependency");
        let lazy_cause = cause.clone();
        host.serve(
            dependency.clone(),
            Rc::new(move |_, _, _| Err(lazy_cause.clone())),
            ServeOptions::default(),
        )
        .unwrap();
        let facet = NativeFacet::new("deferred-failure", vec![], move |_, scope| {
            let dependency = dependency.clone();
            let cause = cause.clone();
            scope.serve(
                token("failure-eager"),
                Rc::new(move |_, _, declaration| {
                    match kind {
                        "factory" => return Err(cause.clone()),
                        "dependency" => {
                            declaration.get(
                                dependency.clone(),
                                json!({}),
                                GetOptions::default(),
                            )?;
                        }
                        "resource" => {
                            let cause = cause.clone();
                            declaration.resource(
                                "refused",
                                ResourceDeclaration::<()>::new(move |_| {
                                    let cause = cause.clone();
                                    async move { Err(cause) }.boxed_local()
                                }),
                            )?;
                        }
                        _ => unreachable!(),
                    }
                    Ok(Implementation::new())
                }),
                eager(),
            )
        })
        .unwrap();
        driver
            .run_until(
                host.install(
                    facet,
                    LoadOptions {
                        await_host_objects: false,
                    },
                )
                .unwrap(),
            )
            .unwrap();
        driver.run_until_stalled();
        assert_eq!(driver.run_until(host.ready()).unwrap_err(), original);
        assert_eq!(driver.run_until(host.ready()).unwrap_err(), original);
        assert_eq!(host.errors(), vec![original]);
        driver.run_until(host.close()).unwrap();
    }
}

thread_local! {
    static SETUP_CANCEL_REENTRY: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}
struct SetupCancelWake;
impl std::task::Wake for SetupCancelWake {
    fn wake(self: std::sync::Arc<Self>) {
        let callback = SETUP_CANCEL_REENTRY.with(|slot| slot.borrow_mut().take());
        if let Some(callback) = callback {
            callback();
        }
    }
}

#[test]
fn setup_cancellation_reentry_cannot_reuse_a_withdrawn_object_for_its_replacement() {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let shared = token("reentrant-service");
    let replacement_host = host.clone();
    let replacement_token = shared.clone();
    let replacements = Rc::new(Counter::new(0));
    let called = replacements.clone();
    SETUP_CANCEL_REENTRY.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            replacement_host
                .serve(
                    replacement_token,
                    Rc::new(move |_, _, _| {
                        called.set(called.get() + 1);
                        Ok(Implementation::new())
                    }),
                    eager(),
                )
                .unwrap();
        }))
    });
    let pending = Rc::new(RefCell::new(None::<LocalBoxFuture<'static, RuntimeError>>));
    let retained = pending.clone();
    let acquire_host = host.clone();
    let saved = Rc::new(RefCell::new(None));
    let acquired = saved.clone();
    let facet = NativeFacet::new("reentrant-failure", vec![], move |scope, declaration| {
        declaration.serve(
            shared.clone(),
            Rc::new(|_, _, _| panic!("withdrawn factory")),
            eager(),
        )?;
        *acquired.borrow_mut() =
            Some(acquire_host.hold(shared.clone(), json!({}), GetOptions::default())?);
        let waker = std::task::Waker::from(std::sync::Arc::new(SetupCancelWake));
        let mut context = TaskContext::from_waker(&waker);
        let mut wait = scope.signal().cancelled();
        assert!(wait.as_mut().poll(&mut context).is_pending());
        *retained.borrow_mut() = Some(wait);
        Err(RuntimeError::new(
            ErrorCode::Forbidden,
            "setup_refused",
            "refused",
        ))
    })
    .unwrap();
    assert_eq!(
        host.install(facet, LoadOptions::default())
            .err()
            .unwrap()
            .reason,
        "setup_refused"
    );
    assert_eq!(
        saved.borrow().as_ref().unwrap().error().unwrap().reason,
        "setup_refused"
    );
    driver.run_until_stalled();
    assert!(
        matches!(host.ready().now_or_never(), Some(Ok(()))),
        "the new service owns a separate completed startup"
    );
    assert_eq!(replacements.get(), 1);
    pending.borrow_mut().take();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn successful_setup_reentrant_hold_shares_the_original_eager_startup_record() {
    for fail in [false, true] {
        let driver = ManualDriver::new();
        let host = host(&driver);
        let requested = token("reentrant-startup");
        let saved = Rc::new(RefCell::new(None));
        let retained = saved.clone();
        let acquired_host = host.clone();
        let mut cause = RuntimeError::new(ErrorCode::Forbidden, "eager_refused", "eager refused");
        cause.set_details(json!({"original":true}));
        let original = cause.clone();
        let facet = NativeFacet::new("reentrant-startup", vec![], move |_, declaration| {
            let cause = cause.clone();
            declaration.serve(
                requested.clone(),
                Rc::new(move |_, _, _| {
                    if fail {
                        Err(cause.clone())
                    } else {
                        Ok(Implementation::new())
                    }
                }),
                eager(),
            )?;
            *retained.borrow_mut() =
                Some(acquired_host.hold(requested.clone(), json!({}), GetOptions::default())?);
            Ok(())
        })
        .unwrap();
        let startup = host.install(facet, LoadOptions::default()).unwrap();
        let ready = host.ready();
        driver.run_until_stalled();
        let startup = startup
            .now_or_never()
            .expect("a successful setup's reentrant object must settle the startup ledger");
        let ready = ready
            .now_or_never()
            .expect("Host.ready must share that same settled ledger");
        let reference = saved.borrow_mut().take().unwrap();
        if fail {
            assert_eq!(startup.unwrap_err(), original);
            assert_eq!(ready.unwrap_err(), original);
            assert_eq!(reference.error().unwrap(), original);
            assert_eq!(host.errors(), vec![original]);
        } else {
            startup.unwrap();
            ready.unwrap();
            assert_eq!(reference.status(), RefStatus::Live);
            reference.release();
            driver.run_until_stalled();
            driver.run_until(host.ready()).unwrap();
            assert!(host.errors().is_empty());
        }
        driver.run_until(host.close()).unwrap();
    }
}

#[test]
fn caught_setup_panic_resumes_prior_work_through_ready_or_existing_object_get() {
    for use_ready in [true, false] {
        let driver = ManualDriver::new();
        let host = host(&driver);
        let requested = token("resumed-after-panic");
        let started = Rc::new(Counter::new(0));
        let observed = started.clone();
        let install = requested.clone();
        let first = NativeFacet::new("successful", vec![], move |_, declaration| {
            let observed = observed.clone();
            declaration.serve(
                install.clone(),
                Rc::new(move |_, _, _| {
                    observed.set(observed.get() + 1);
                    Ok(Implementation::new())
                }),
                eager(),
            )
        })
        .unwrap();
        let second = NativeFacet::new("panicking", vec![], |_, _| panic!("setup panic")).unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = host.load(vec![first, second], LoadOptions::default());
            }))
            .is_err()
        );
        assert_eq!(started.get(), 0, "no factory executes during unwind");
        let acquiring = host.clone();
        let mut wait: LocalBoxFuture<'static, Result<()>> = if use_ready {
            host.ready()
        } else {
            async move {
                acquiring
                    .get(requested, json!({}), GetOptions::default())
                    .await
                    .map(|_| ())
            }
            .boxed_local()
        };
        let mut context = TaskContext::from_waker(futures::task::noop_waker_ref());
        assert!(wait.as_mut().poll(&mut context).is_pending());
        driver.run_until_stalled();
        wait.now_or_never()
            .expect("ordinary readiness/acquisition must resume prior queued work")
            .unwrap();
        assert_eq!(started.get(), 1);
        driver.run_until(host.close()).unwrap();
    }
}

#[test]
fn retried_host_singleton_remains_pinned_while_original_startup_failure_stays_selected() {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let requested = token("retry-host-singleton");
    let registry = Registry::new("retried-contributions", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let attempts = Rc::new(Counter::new(0));
    let observed = attempts.clone();
    let closed = Rc::new(Counter::new(0));
    let cleanup = closed.clone();
    let cause = RuntimeError::new(
        ErrorCode::Forbidden,
        "first_startup",
        "first startup refused",
    );
    let original = cause.clone();
    host.serve(
        requested.clone(),
        Rc::new(move |_, _, declaration| {
            observed.set(observed.get() + 1);
            if observed.get() == 1 {
                return Err(cause.clone());
            }
            declaration.register(&registry, "retained", json!("retry"))?;
            let closed = cleanup.clone();
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
        eager(),
    )
    .unwrap();
    assert_eq!(driver.run_until(host.ready()).unwrap_err(), original);
    driver.run_until_stalled();
    let retried = driver
        .run_until(host.get(requested, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(attempts.get(), 2);
    retried.release();
    driver.run_until_stalled();
    assert_eq!(
        closed.get(),
        0,
        "every successful host singleton has a lifetime pin independent of claims"
    );
    assert_eq!(
        view.table(None).unwrap().get("retained").unwrap().data(),
        &json!("retry")
    );
    assert_eq!(driver.run_until(host.ready()).unwrap_err(), original);
    assert_eq!(host.errors(), vec![original]);
    driver.run_until(host.close()).unwrap();
    assert_eq!(closed.get(), 1);
}

#[test]
fn captured_reentrant_startup_waiters_settle_on_close_after_physical_open_cleanup() {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let requested = token("closing-reentrant-startup");
    let (complete, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    let closes = Rc::new(Counter::new(0));
    let factory_closes = closes.clone();
    let captured_host = host.clone();
    let saved = Rc::new(RefCell::new(None));
    let retained = saved.clone();
    let facet = NativeFacet::new(
        "closing-reentrant-startup",
        vec![],
        move |_, declaration| {
            let receive = receive.clone();
            let closes = factory_closes.clone();
            declaration.serve(
                requested.clone(),
                Rc::new(move |_, _, declaration| {
                    let receive = receive.clone();
                    let closes = closes.clone();
                    declaration.resource(
                        "late",
                        ResourceDeclaration::new(move |_| {
                            let receive = receive.borrow_mut().take().unwrap();
                            async move {
                                receive.await.unwrap();
                                Ok(())
                            }
                            .boxed_local()
                        })
                        .with_close(move |_| {
                            closes.set(closes.get() + 1);
                            async { Ok(()) }.boxed_local()
                        }),
                    )?;
                    Ok(Implementation::new())
                }),
                eager(),
            )?;
            *retained.borrow_mut() =
                Some(captured_host.hold(requested.clone(), json!({}), GetOptions::default())?);
            Ok(())
        },
    )
    .unwrap();
    let startup = host.install(facet, LoadOptions::default()).unwrap();
    let captured_ready = host.ready();
    let (send, result) = oneshot::channel();
    driver.spawn(
        async move {
            let outcomes = futures::join!(startup, captured_ready);
            let _ = send.send(outcomes);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(host.close().now_or_never().is_none());
    driver.run_until_stalled();
    assert_eq!(
        closes.get(),
        0,
        "cancellation does not close an opener still using its resource"
    );
    complete.send(()).unwrap();
    driver.run_until(host.close()).unwrap();
    driver.run_until_stalled();
    let (startup, ready) = result
        .now_or_never()
        .expect("captured waiters must settle even after the ledger is cleared")
        .unwrap();
    let errors = host.errors();
    assert_eq!(
        errors.len(),
        1,
        "expected close cancellation is a retained unsuccessful eager startup once"
    );
    let cause = &errors[0];
    assert!(matches!(cause.code, ErrorCode::Aborted | ErrorCode::Gone));
    assert_eq!(&startup.unwrap_err(), cause);
    assert_eq!(&ready.unwrap_err(), cause);
    assert_eq!(closes.get(), 1);
    driver.run_until(host.close()).unwrap();
    assert_eq!(host.errors(), errors);
    assert_eq!(closes.get(), 1);
}
