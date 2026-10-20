use futures::{FutureExt, future::pending};
use serde_json::json;
use std::{
    cell::{Cell as Count, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};

fn token(name: &str) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            )]),
        )
        .unwrap(),
    )
}
fn host(driver: &Rc<ManualDriver>) -> Host {
    Host::new(HostOptions::new("execution", json!(null)), driver.clone())
}

#[test]
fn physical_leases_outlive_dropped_waiter_but_never_extend_context_admission() {
    for portable in [false, true] {
        physical_lease_case(portable);
    }
}
fn physical_lease_case(portable: bool) {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let contract = token("leased");
    let leases = Rc::new(RefCell::new(Vec::<InvocationLease>::new()));
    let saved = Rc::new(RefCell::new(None::<Context>));
    let owner = Rc::new(RefCell::new(None::<InvocationOwner>));
    let closed = Rc::new(Count::new(0));
    let (jobs, contexts, owners, closes) =
        (leases.clone(), saved.clone(), owner.clone(), closed.clone());
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let owner = declaration.invocation_owner();
            *owners.borrow_mut() = Some(owner.clone());
            let (jobs, contexts) = (jobs.clone(), contexts.clone());
            let closes = closes.clone();
            declaration.resource(
                "resource",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        closes.set(closes.get() + 1);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            let invoke = move |context: Context| {
                jobs.borrow_mut().push(owner.retain(&context).unwrap());
                jobs.borrow_mut().push(owner.retain(&context).unwrap());
                *contexts.borrow_mut() = Some(context);
                pending::<Result<()>>().boxed_local()
            };
            Ok(if portable {
                Implementation::new().json_method("run", move |_, context| {
                    invoke(context)
                        .map(|result| result.map(|()| None))
                        .boxed_local()
                })
            } else {
                Implementation::new().method("run", move |(): (), context| invoke(context))
            })
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    // Construction alone enters neither handler nor physical retention.
    if portable {
        drop(reference.call_json("run", None, None));
    } else {
        drop(reference.call::<(), ()>("run", (), None));
    }
    assert!(leases.borrow().is_empty());
    let mut call = if portable {
        reference
            .call_json("run", None, None)
            .map(|result| result.map(|_| ()))
            .boxed_local()
    } else {
        reference.call::<(), ()>("run", (), None).boxed_local()
    };
    let initial = call.as_mut().now_or_never();
    assert!(initial.is_none(), "portable={portable}: {initial:?}");
    assert_eq!(leases.borrow().len(), 2);
    drop(call);
    let context = saved.borrow().clone().unwrap();
    assert_eq!(context.check(host.id()).unwrap_err().reason, "expired");
    assert_eq!(
        owner
            .borrow()
            .as_ref()
            .unwrap()
            .retain(&context)
            .err()
            .unwrap()
            .reason,
        "expired"
    );
    reference.release();
    let mut closing = host.close().boxed_local();
    assert!(closing.as_mut().now_or_never().is_none());
    driver.run_until_stalled();
    assert_eq!(closed.get(), 0);
    drop(leases.borrow_mut().pop());
    driver.run_until_stalled();
    assert_eq!(
        closed.get(),
        0,
        "one remaining share retains the same physical entry"
    );
    let final_lease = leases.borrow_mut().pop();
    drop(final_lease);
    driver.run_until(closing).unwrap();
    assert_eq!(closed.get(), 1);
}

#[test]
fn owner_requires_the_exact_entered_receiver_and_preserves_cancellation() {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let a = token("owner-a");
    let b = token("owner-b");
    let saved = Rc::new(RefCell::new(None::<InvocationOwner>));
    let original = saved.clone();
    host.serve(
        a.clone(),
        Rc::new(move |_, _, declaration| {
            *original.borrow_mut() = Some(declaration.invocation_owner());
            Ok(Implementation::new().method("run", |(): (), _| async { Ok(()) }.boxed_local()))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let a_ref = driver
        .run_until(host.get(a, json!({}), GetOptions::default()))
        .unwrap();
    let other = saved.borrow().clone().unwrap();
    let checks = Rc::new(Count::new(0));
    let observed = checks.clone();
    host.serve(
        b.clone(),
        Rc::new(move |_, _, declaration| {
            let own = declaration.invocation_owner();
            let (other, checks) = (other.clone(), observed.clone());
            Ok(Implementation::new().method("run", move |(): (), context| {
                assert_eq!(
                    other.retain(&context).err().unwrap().reason,
                    "invocation_owner"
                );
                let (descendant, _guard) = context.child_call(json!(null)).unwrap();
                assert_eq!(
                    own.retain(&descendant).err().unwrap().reason,
                    "invocation_context"
                );
                let lease = own.retain(&context).unwrap();
                context.cancel(
                    RuntimeError::new(ErrorCode::Forbidden, "denied", "denied")
                        .with_details(json!({"cause": 1})),
                );
                let error = own.retain(&context).err().unwrap();
                assert_eq!(
                    (error.code, error.reason.as_str(), &error.details()["cause"]),
                    (ErrorCode::Aborted, "denied", &json!(1))
                );
                drop(lease);
                checks.set(checks.get() + 1);
                async { Ok(()) }.boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let b_ref = driver
        .run_until(host.get(b, json!({}), GetOptions::default()))
        .unwrap();
    driver
        .run_until(b_ref.call::<(), ()>("run", (), None))
        .unwrap();
    assert_eq!(checks.get(), 1);
    let owner = saved.borrow().clone().unwrap();
    let root_owner = owner.clone();
    driver
        .run_until(host.root(move |context| {
            async move {
                assert_eq!(
                    root_owner.retain(&context).err().unwrap().reason,
                    "invocation_context"
                );
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    let foreign = Host::new(HostOptions::new("foreign", json!(null)), driver.clone());
    driver
        .run_until(foreign.root(move |context| {
            async move {
                assert_eq!(owner.retain(&context).err().unwrap().reason, "foreign_ctx");
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    a_ref.release();
    b_ref.release();
    driver.run_until(host.close()).unwrap();
    driver.run_until(foreign.close()).unwrap();
}

#[test]
fn object_recorder_checks_immediately_and_expires_after_return_or_error() {
    for fail in [false, true] {
        let driver = ManualDriver::new();
        let host = host(&driver);
        let contract = token("recorder");
        let stored = Rc::new(RefCell::new(None::<DeclarationRecorder>));
        let record = stored.clone();
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let recorder = declaration.recorder();
                let cell = recorder.state(
                    "cell",
                    StateDeclaration::new(json!({"type":"integer"}), json!(3)),
                )?;
                assert_eq!(cell.snapshot()?.value(), &json!(3));
                assert_eq!(
                    recorder
                        .state("cell", StateDeclaration::new(json!({}), json!(0)))
                        .err()
                        .unwrap()
                        .reason,
                    "duplicate"
                );
                let derived = recorder.derive(
                    vec![cell.readable()],
                    json!({"type":"integer"}),
                    |values| Ok(json!(values[0].value().as_i64().unwrap() * 2)),
                )?;
                assert_eq!(derived.snapshot()?.value(), &json!(6));
                let resource = recorder.resource(
                    "resource",
                    ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()),
                )?;
                assert_eq!(resource.value().err().unwrap().reason, "factory");
                let _owner = recorder.invocation_owner()?;
                *record.borrow_mut() = Some(recorder);
                if fail {
                    return Err(RuntimeError::invalid("factory_failed", "factory failed"));
                }
                Ok(Implementation::new().method("run", |(): (), _| async { Ok(()) }.boxed_local()))
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let result = driver.run_until(host.get(contract, json!({}), GetOptions::default()));
        if fail {
            assert_eq!(result.err().unwrap().reason, "factory_failed");
        }
        let recorder = stored.borrow().clone().unwrap();
        assert_eq!(
            recorder
                .state("late", StateDeclaration::new(json!({}), json!(1)))
                .err()
                .unwrap()
                .reason,
            "declaration_scope"
        );
        driver.run_until(host.close()).unwrap();
    }
}

#[test]
fn foreign_and_native_facets_share_one_installer_and_exact_setup_passes() {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let outer_token = token("outer-service");
    let nested_token = token("nested-service");
    let captured = Rc::new(RefCell::new(None::<FacetRecorder>));
    let saved = captured.clone();
    let nested_host = host.clone();
    let foreign = ExecutableFacet::foreign(
        "foreign",
        vec![outer_token.clone()],
        move |_, declaration| {
            let recorder = declaration.recorder();
            *saved.borrow_mut() = Some(recorder.clone());
            let outer = recorder.clone();
            let contract = nested_token.clone();
            let nested_startup = nested_host.install(
                NativeFacet::new("native", vec![], move |_, inner| {
                    assert_eq!(
                        outer
                            .serve(contract.clone(), empty_factory(), ServeOptions::default())
                            .unwrap_err()
                            .reason,
                        "setup_scope"
                    );
                    inner.recorder().serve(
                        contract.clone(),
                        empty_factory(),
                        ServeOptions::default(),
                    )
                })?,
                LoadOptions::default(),
            )?;
            drop(nested_startup);
            recorder.serve(
                outer_token.clone(),
                empty_factory(),
                ServeOptions::default(),
            )
        },
    )
    .unwrap();
    assert_eq!(foreign.kind(), FacetKind::Foreign);
    assert_eq!(foreign.uses().len(), 1);
    let native: ExecutableFacet = NativeFacet::new("plain-native", vec![], |_, _| Ok(()))
        .unwrap()
        .into();
    assert_eq!(native.kind(), FacetKind::Native);
    let startup = host
        .load_executable(vec![foreign.clone(), native], LoadOptions::default())
        .unwrap();
    driver.run_until(startup).unwrap();
    assert_eq!(
        host.install_executable(foreign, LoadOptions::default())
            .err()
            .unwrap()
            .reason,
        "duplicate_facet"
    );
    let recorder = captured.borrow().clone().unwrap();
    assert_eq!(
        recorder
            .serve(token("late"), empty_factory(), ServeOptions::default())
            .unwrap_err()
            .reason,
        "setup_scope"
    );
    driver
        .run_until(host.get(token("outer-service"), json!({}), GetOptions::default()))
        .unwrap();
    driver
        .run_until(host.get(token("nested-service"), json!({}), GetOptions::default()))
        .unwrap();
    driver.run_until(host.close()).unwrap();
}

fn empty_factory() -> Factory {
    Rc::new(|_, _, _| {
        Ok(Implementation::new().method("run", |(): (), _| async { Ok(()) }.boxed_local()))
    })
}

#[test]
fn rejected_factory_capture_drop_can_reenter_the_same_facet_recorder() {
    struct Reenter {
        recorder: FacetRecorder,
        calls: Rc<Count<u32>>,
    }
    impl Drop for Reenter {
        fn drop(&mut self) {
            self.recorder
                .serve(token("from-drop"), empty_factory(), ServeOptions::default())
                .unwrap();
            self.calls.set(self.calls.get() + 1);
        }
    }
    let driver = ManualDriver::new();
    let host = host(&driver);
    let calls = Rc::new(Count::new(0));
    let drops = calls.clone();
    let facet = ExecutableFacet::foreign("reentrant", vec![], move |_, declaration| {
        let recorder = declaration.recorder();
        recorder.serve(token("duplicate"), empty_factory(), ServeOptions::default())?;
        let capture = Reenter {
            recorder: recorder.clone(),
            calls: drops.clone(),
        };
        let rejected: Factory = Rc::new(move |_, _, _| {
            let _keep = &capture;
            Ok(Implementation::new())
        });
        assert_eq!(
            recorder
                .serve(token("duplicate"), rejected, ServeOptions::default())
                .unwrap_err()
                .reason,
            "duplicate"
        );
        Ok(())
    })
    .unwrap();
    driver
        .run_until(
            host.install_executable(facet, LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(calls.get(), 1);
    driver
        .run_until(host.get(token("from-drop"), json!({}), GetOptions::default()))
        .unwrap();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn logical_scope_expires_before_last_entry_can_start_reentrant_cleanup() {
    struct EagerDriver(Rc<ManualDriver>);
    impl Driver for EagerDriver {
        fn spawn(&self, mut future: futures::future::LocalBoxFuture<'static, ()>) {
            let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
            if future.as_mut().poll(&mut context).is_pending() {
                self.0.spawn(future);
            }
        }
        fn sleep(
            &self,
            duration: std::time::Duration,
        ) -> futures::future::LocalBoxFuture<'static, ()> {
            self.0.sleep(duration)
        }
        fn now(&self) -> std::time::Duration {
            self.0.now()
        }
    }
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("eager", json!(null)),
        Rc::new(EagerDriver(driver.clone())),
    );
    let contract = token("scope-order");
    let held = Rc::new(RefCell::new(None::<Reference>));
    let saved = Rc::new(RefCell::new(None::<Context>));
    let closed = Rc::new(Count::new(0));
    let (claim, entered, closes) = (held.clone(), saved.clone(), closed.clone());
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let (claim, entered) = (claim.clone(), entered.clone());
            let contexts = entered.clone();
            let closes = closes.clone();
            declaration.resource(
                "resource",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        let context = contexts.borrow().clone().unwrap();
                        assert_eq!(
                            context.check(context.host_id()).unwrap_err().reason,
                            "expired"
                        );
                        closes.set(closes.get() + 1);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            Ok(Implementation::new().method("run", move |(): (), context| {
                *entered.borrow_mut() = Some(context);
                claim.borrow().as_ref().unwrap().release();
                async { Ok(()) }.boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    *held.borrow_mut() = Some(reference.clone());
    driver
        .run_until(reference.call::<(), ()>("run", (), None))
        .unwrap();
    assert_eq!(
        closed.get(),
        1,
        "eager cleanup ran from the final entry release"
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn setup_recorder_expires_during_unwind_before_later_installation() {
    let driver = ManualDriver::new();
    let host = host(&driver);
    let stored = Rc::new(RefCell::new(None::<FacetRecorder>));
    let retained = stored.clone();
    let facet = ExecutableFacet::foreign("unwind", vec![], move |_, declaration| {
        *retained.borrow_mut() = Some(declaration.recorder());
        declaration.recorder().serve(
            token("withdrawn"),
            empty_factory(),
            ServeOptions::default(),
        )?;
        panic!("native test setup panic");
    })
    .unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _startup = host.install_executable(facet, LoadOptions::default());
        }))
        .is_err()
    );
    assert_eq!(
        stored
            .borrow()
            .as_ref()
            .unwrap()
            .serve(token("late"), empty_factory(), ServeOptions::default())
            .unwrap_err()
            .reason,
        "setup_scope"
    );
    let replacement = NativeFacet::new("unwind", vec![], |_, declaration| {
        declaration.serve(token("withdrawn"), empty_factory(), ServeOptions::default())
    })
    .unwrap();
    driver
        .run_until(host.install(replacement, LoadOptions::default()).unwrap())
        .unwrap();
    driver
        .run_until(host.get(token("withdrawn"), json!({}), GetOptions::default()))
        .unwrap();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn completed_or_failed_logical_calls_keep_detached_work_physically_owned() {
    for failed in [false, true] {
        let driver = ManualDriver::new();
        let host = host(&driver);
        let contract = token("detached");
        let retained = Rc::new(RefCell::new(None::<InvocationLease>));
        let saved = Rc::new(RefCell::new(None::<Context>));
        let closed = Rc::new(Count::new(0));
        let (job, context, closes) = (retained.clone(), saved.clone(), closed.clone());
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let owner = declaration.invocation_owner();
                let (job, context) = (job.clone(), context.clone());
                let closes = closes.clone();
                declaration.resource(
                    "resource",
                    ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                        move |_| {
                            closes.set(closes.get() + 1);
                            async { Ok(()) }.boxed_local()
                        },
                    ),
                )?;
                Ok(Implementation::new().method("run", move |(): (), ctx| {
                    *job.borrow_mut() = Some(owner.retain(&ctx).unwrap());
                    *context.borrow_mut() = Some(ctx);
                    async move {
                        if failed {
                            Err(RuntimeError::invalid(
                                "callback",
                                "callback failed after entry",
                            ))
                        } else {
                            Ok(())
                        }
                    }
                    .boxed_local()
                }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let reference = driver
            .run_until(host.get(contract, json!({}), GetOptions::default()))
            .unwrap();
        let result = driver.run_until(reference.call::<(), ()>("run", (), None));
        assert_eq!(result.is_err(), failed);
        assert_eq!(
            saved
                .borrow()
                .as_ref()
                .unwrap()
                .check(host.id())
                .unwrap_err()
                .reason,
            "expired"
        );
        reference.release();
        let mut closing = host.close().boxed_local();
        assert!(closing.as_mut().now_or_never().is_none());
        driver.run_until_stalled();
        assert_eq!(closed.get(), 0);
        let lease = retained.borrow_mut().take();
        drop(lease);
        driver.run_until(closing).unwrap();
        assert_eq!(closed.get(), 1);
    }
}
