#![cfg(feature = "hooks")]
use futures::{FutureExt, channel::oneshot, executor::LocalPool};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
    task::Poll,
};
use tembra::{
    Code, Context, Error, Hook, Host, Orb, Service,
    core::{ContextKey, OpaqueContextValue, Requirement},
};

const TRACE: ContextKey<String> = ContextKey::new("test/trace", 1);
const STORAGE: ContextKey<OpaqueContextValue> = ContextKey::new("test/storage", 1);

fn requirement() -> Requirement {
    Requirement {
        orb: "provider".into(),
        api_version: 1,
        methods: Default::default(),
        hooks: ["terminal".into()].into(),
    }
}

#[test]
fn terminal_observers_are_awaited_with_principal_and_normal_work_after_cancellation() {
    for primary in ["Completed", "Failed", "Cancelled"] {
        for observer_fails in [false, true] {
            let mut pool = LocalPool::new();
            let host = Host::new(pool.spawner());
            let hook = Hook::<String, ()>::series("terminal");
            let declared = hook.clone();
            let seen = Rc::new(RefCell::new(Vec::new()));
            let captured = seen.clone();
            let original = Rc::new(RefCell::new(None::<Context>));
            let retained = original.clone();
            host.install(vec![
                Orb::new("provider", 1)
                    .hook(hook.clone())
                    .service(Service::new("dependency", |_| Ok(())).method(
                        "read",
                        |_, ctx, (): ()| async move {
                            assert_eq!(ctx.principal()?, "alice");
                            Ok(42)
                        },
                    ))
                    .service(
                        Service::new("provider", move |self_| {
                            Ok((self_.hook(&declared)?, retained.clone()))
                        })
                        .method(
                            "run",
                            |env, ctx, primary: String| async move {
                                let ctx = ctx.with(&TRACE, "trace".into())?.with(
                                    &STORAGE,
                                    OpaqueContextValue::new(1, "owner-issued storage lease"),
                                )?;
                                *env.1.borrow_mut() = Some(ctx.clone());
                                assert_eq!(ctx.value(&TRACE)?, Some("trace".into()));
                                if primary == "Cancelled" {
                                    ctx.cancel()?;
                                    assert_eq!(ctx.check().unwrap_err().code, Code::Cancelled);
                                }
                                let outcome = primary.clone();
                                let result = env.0.observe(&ctx, outcome).await;
                            if primary == "Cancelled" {
                                assert_eq!(ctx.check().unwrap_err().code, Code::Cancelled);
                                assert!(matches!(ctx.get("dependency", vec!["main".into()], Default::default()), Err(error) if error.code == Code::Cancelled));
                            } else {
                                ctx.check()?;
                            }
                            Ok((primary, result.err().map(|error| error.reason)))
                            },
                        ),
                    ),
                Orb::new("observer", 1).requires(requirement()).contribute(
                    "provider",
                    &hook,
                    0,
                    move |ctx, outcome| {
                        let captured = captured.clone();
                        async move {
                            assert_eq!(ctx.principal()?, "alice");
                            assert_eq!(ctx.value(&TRACE)?, None);
                            assert_eq!(ctx.value(&STORAGE)?, None);
                            let dependency =
                                ctx.get("dependency", vec!["main".into()], Default::default())?;
                            let value: i32 = dependency.call(&ctx, "read", ()).await?;
                            captured.borrow_mut().push((outcome, value));
                            if observer_fails {
                                Err(Error::invalid("observer_failure", "observer failed"))
                            } else {
                                Ok(())
                            }
                        }
                    },
                ),
            ])
            .unwrap();
            let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
            let input = primary.to_string();
            let result: (String, Option<String>) = pool
                .run_until(host.root("alice", move |ctx| {
                    async move { reference.call(&ctx, "run", input).await }.boxed_local()
                }))
                .unwrap();
            assert_eq!(result.0, primary);
            assert_eq!(
                result.1.as_deref(),
                observer_fails.then_some("observer_failure")
            );
            assert_eq!(*seen.borrow(), [(primary.to_string(), 42)]);
            let expired = original.borrow().clone().unwrap();
            assert!(expired.check().is_err());
            assert_eq!(host.kernel().diagnostics().calls, 0);
            assert_eq!(host.kernel().diagnostics().roots, 0);
            assert_eq!(host.kernel().context_stats().0, 0);
            pool.run_until(host.close()).unwrap();
            assert_eq!(host.kernel().diagnostics().resources, 0);
        }
    }
}

#[test]
fn terminal_observers_use_the_source_hook_plan_after_publication() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<String, ()>::series("terminal");
    let declared = hook.clone();
    let retained_host = host.clone();
    let late_hook = hook.clone();
    let old_called = Rc::new(Cell::new(false));
    let late_called = Rc::new(Cell::new(false));
    let old = old_called.clone();
    let late = late_called.clone();
    host.install(vec![
        Orb::new("provider", 1).hook(hook.clone()).service(
            Service::new("provider", move |self_| {
                Ok((
                    self_.hook(&declared)?,
                    retained_host.clone(),
                    late_hook.clone(),
                    late.clone(),
                ))
            })
            .method("run", |env, ctx, (): ()| async move {
                let retained_version = ctx.invocation()?.code_version;
                let late = env.3.clone();
                env.1.install(vec![
                    Orb::new("late", 1).requires(requirement()).contribute(
                        "provider",
                        &env.2,
                        0,
                        move |_, _| {
                            late.set(true);
                            async { Ok(()) }
                        },
                    ),
                ])?;
                assert!(env.1.kernel().version() > retained_version);
                ctx.cancel()?;
                env.0.observe(&ctx, "Cancelled".into()).await
            }),
        ),
        Orb::new("old", 1)
            .requires(requirement())
            .contribute("provider", &hook, 0, move |_, _| {
                old.set(true);
                async { Ok(()) }
            }),
    ])
    .unwrap();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    pool.run_until(host.root("alice", move |ctx| {
        async move { reference.call::<_, ()>(&ctx, "run", ()).await }.boxed_local()
    }))
    .unwrap();
    assert!(old_called.get());
    assert!(!late_called.get());
    pool.run_until(host.close()).unwrap();
}

#[test]
fn a_failed_execution_keeps_its_primary_error_when_terminal_observation_also_fails() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<String, ()>::series("terminal");
    let declared = hook.clone();
    let observer_error = Rc::new(RefCell::new(None::<String>));
    let retained_error = observer_error.clone();
    let observed_failure = Rc::new(Cell::new(false));
    let captured_failure = observed_failure.clone();
    host.install(vec![
        Orb::new("provider", 1).hook(hook.clone()).service(
            Service::new("provider", move |self_| {
                Ok((self_.hook(&declared)?, retained_error.clone()))
            })
            .method("run", |env, ctx, (): ()| async move {
                let primary = Err::<(), _>(Error::invalid("execution_failure", "execution failed"));
                let observer_error = env.1.clone();
                let terminal = env.0.observe(&ctx, "Failed".into()).await;
                *observer_error.borrow_mut() = terminal.err().map(|error| error.reason);
                primary
            }),
        ),
        Orb::new("observer", 1).requires(requirement()).contribute(
            "provider",
            &hook,
            0,
            move |ctx, outcome| {
                let captured_failure = captured_failure.clone();
                async move {
                    assert_eq!(outcome, "Failed");
                    assert_eq!(ctx.principal()?, "alice");
                    captured_failure.set(true);
                    Err(Error::invalid("observer_failure", "observer failed"))
                }
            },
        ),
    ])
    .unwrap();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    let primary = pool.run_until(host.root("alice", move |ctx| {
        async move { reference.call::<_, ()>(&ctx, "run", ()).await }.boxed_local()
    }));
    assert_eq!(primary.unwrap_err().reason, "execution_failure");
    assert!(observed_failure.get());
    assert_eq!(observer_error.borrow().as_deref(), Some("observer_failure"));
    assert_eq!(host.kernel().diagnostics().roots, 0);
    assert_eq!(host.kernel().diagnostics().calls, 0);
    assert!(host.errors().is_empty());
    pool.run_until(host.close()).unwrap();
}

#[test]
fn admission_is_bound_once_to_the_source_owner_and_cannot_be_reopened() {
    use std::{collections::BTreeMap, time::Duration};
    use tembra::HookHandle;
    type Handles = Rc<RefCell<BTreeMap<String, HookHandle<(), ()>>>>;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let terminal = Hook::<(), ()>::series("terminal");
    let declared = terminal.clone();
    let descendant = terminal.clone();
    let handles: Handles = Rc::new(RefCell::new(BTreeMap::new()));
    let retained = handles.clone();
    let reopened = handles.clone();
    let calls = Rc::new(Cell::new(0));
    let called = calls.clone();
    let source = Rc::new(RefCell::new(None::<Context>));
    let retained_source = source.clone();
    host.install(vec![
        Orb::new("provider", 1)
            .hook(terminal.clone())
            .service(
                Service::new("provider", move |self_| {
                    let hook = self_.hook(&declared)?;
                    retained
                        .borrow_mut()
                        .insert(self_.address()?.key[0].clone(), hook.clone());
                    Ok((
                        hook,
                        self_.hook(&declared)?,
                        retained.clone(),
                        retained_source.clone(),
                    ))
                })
                .method("run", |env, ctx, (): ()| async move {
                    *env.3.borrow_mut() = Some(ctx.clone());
                    let wrong = env.2.borrow().get("other").unwrap().clone();
                    assert!(wrong.observe(&ctx, ()).await.is_err());
                    ctx.cancel()?;
                    let (first, competing) =
                        futures::join!(env.0.observe(&ctx, ()), env.1.observe(&ctx, ()));
                    let reasons = [first.unwrap_err().reason, competing.unwrap_err().reason];
                    assert_eq!(
                        reasons
                            .iter()
                            .filter(|reason| *reason == "observer_failure")
                            .count(),
                        1
                    );
                    assert!(env.0.observe(&ctx, ()).await.is_err());
                    Ok(())
                }),
            )
            .service(
                Service::new("descendant", move |self_| self_.hook(&descendant)).method(
                    "reopen",
                    |hook, ctx, (): ()| async move {
                        assert!(hook.observe(&ctx, ()).await.is_err());
                        Ok(())
                    },
                ),
            )
            .contribute("provider", &terminal, 0, move |ctx, ()| {
                let reopened = reopened.clone();
                let called = called.clone();
                async move {
                    called.set(called.get() + 1);
                    let hook = reopened.borrow().get("main").unwrap().clone();
                    assert!(hook.observe(&ctx, ()).await.is_err());
                    let dependency =
                        ctx.get("descendant", vec!["main".into()], Default::default())?;
                    dependency.call::<_, ()>(&ctx, "reopen", ()).await?;
                    futures_timer::Delay::new(Duration::from_millis(1)).await;
                    Err(Error::invalid("observer_failure", "observation failed"))
                }
            }),
    ])
    .unwrap();
    let main = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    let other = host.get("provider", vec!["other".into()], "alice").unwrap();
    let hook = handles.borrow().get("main").unwrap().clone();
    let bare = hook.clone();
    pool.run_until(host.root("alice", move |ctx| {
        async move {
            assert!(bare.observe(&ctx, ()).await.is_err());
            ctx.check()?;
            main.call::<_, ()>(&ctx, "run", ()).await
        }
        .boxed_local()
    }))
    .unwrap();
    assert_eq!(calls.get(), 1);
    let expired = source.borrow().clone().unwrap();
    assert!(pool.run_until(hook.observe(&expired, ())).is_err());
    other.release().unwrap();
    pool.run_until(host.close()).unwrap();
    let diagnostics = host.kernel().diagnostics();
    assert_eq!(
        (
            diagnostics.roots,
            diagnostics.calls,
            diagnostics.references,
            diagnostics.resources
        ),
        (0, 0, 0, 0)
    );
}

#[test]
fn dropped_observation_wait_retains_physical_work_until_shutdown_settles() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<(), ()>::series("terminal");
    let declared = hook.clone();
    let (send, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    let terminal = Rc::new(RefCell::new(None::<Context>));
    let captured = terminal.clone();
    let finished = Rc::new(Cell::new(false));
    let settled = finished.clone();
    host.install(vec![
        Orb::new("provider", 1)
            .hook(hook.clone())
            .service(
                Service::new("provider", move |self_| self_.hook(&declared)).method(
                    "run",
                    |hook, ctx, (): ()| async move {
                        ctx.cancel()?;
                        let mut waiting = Box::pin(hook.observe(&ctx, ()));
                        futures::future::poll_fn(|cx| {
                            assert!(waiting.as_mut().poll(cx).is_pending());
                            Poll::Ready(())
                        })
                        .await;
                        drop(waiting);
                        Ok(())
                    },
                ),
            )
            .contribute("provider", &hook, 0, move |ctx, ()| {
                let receive = receive.borrow_mut().take().unwrap();
                let captured = captured.clone();
                let settled = settled.clone();
                async move {
                    *captured.borrow_mut() = Some(ctx.clone());
                    receive.await.unwrap();
                    assert!(ctx.check().is_err());
                    settled.set(true);
                    Ok(())
                }
            }),
    ])
    .unwrap();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    pool.run_until(host.root("alice", {
        let reference = reference.clone();
        move |ctx| async move { reference.call::<_, ()>(&ctx, "run", ()).await }.boxed_local()
    }))
    .unwrap();
    pool.run_until_stalled();
    let terminal = terminal.borrow().clone().unwrap();
    terminal.check().unwrap();
    reference.release().unwrap();
    let mut closing = Box::pin(host.close());
    pool.run_until(futures::future::poll_fn(|cx| {
        assert!(closing.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    assert!(terminal.signal().is_cancelled());
    assert!(!finished.get());
    send.send(()).unwrap();
    pool.run_until(closing).unwrap();
    assert!(finished.get());
    let diagnostics = host.kernel().diagnostics();
    assert_eq!(
        (
            diagnostics.roots,
            diagnostics.calls,
            diagnostics.objects,
            diagnostics.resources
        ),
        (0, 0, 0, 0)
    );
    assert!(host.errors().is_empty());
}

#[test]
fn an_observer_panic_settles_managed_work_and_consumes_the_admission() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<(), ()>::series("terminal");
    let declared = hook.clone();
    host.install(vec![
        Orb::new("provider", 1)
            .hook(hook.clone())
            .service(
                Service::new("provider", move |self_| self_.hook(&declared)).method(
                    "run",
                    |hook, ctx, (): ()| async move {
                        assert!(hook.observe(&ctx, ()).await.is_err());
                        assert!(hook.observe(&ctx, ()).await.is_err());
                        Ok(())
                    },
                ),
            )
            .contribute("provider", &hook, 0, |_, ()| async {
                panic!("observer panic")
            }),
    ])
    .unwrap();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    pool.run_until(host.root("alice", move |ctx| {
        async move { reference.call::<_, ()>(&ctx, "run", ()).await }.boxed_local()
    }))
    .unwrap();
    pool.run_until(host.close()).unwrap();
    let diagnostics = host.kernel().diagnostics();
    assert_eq!(
        (diagnostics.roots, diagnostics.calls, diagnostics.resources),
        (0, 0, 0)
    );
}

#[test]
fn source_cleanup_waits_for_a_detached_observer_descendant_after_the_plan_finishes() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<(), ()>::series("terminal");
    let declared = hook.clone();
    let (send, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    let started = Rc::new(Cell::new(false));
    let entered = started.clone();
    let closed = Rc::new(Cell::new(false));
    let disposed = closed.clone();
    host.install(vec![
        Orb::new("provider", 1)
            .hook(hook.clone())
            .service(
                Service::new("provider", move |self_| {
                    let disposed = disposed.clone();
                    self_.resource(
                        "source resource",
                        1,
                        || Ok(()),
                        move |_, _| {
                            async move {
                                disposed.set(true);
                                Ok(())
                            }
                            .boxed_local()
                        },
                    )?;
                    self_.hook(&declared)
                })
                .method("run", |hook, ctx, (): ()| async move {
                    ctx.cancel()?;
                    hook.observe(&ctx, ()).await
                }),
            )
            .service(
                Service::new("child", move |_| Ok((receive.clone(), entered.clone()))).method(
                    "wait",
                    |state, _, (): ()| async move {
                        state.1.set(true);
                        let receive = state.0.borrow_mut().take().unwrap();
                        receive.await.unwrap();
                        Ok(())
                    },
                ),
            )
            .contribute("provider", &hook, 0, |ctx, ()| async move {
                let child = ctx.get("child", vec!["main".into()], Default::default())?;
                let mut wait = Box::pin(child.call::<_, ()>(&ctx, "wait", ()));
                futures::future::poll_fn(|cx| {
                    assert!(wait.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                drop(wait);
                Ok(())
            }),
    ])
    .unwrap();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    pool.run_until(host.root("alice", {
        let reference = reference.clone();
        move |ctx| async move { reference.call::<_, ()>(&ctx, "run", ()).await }.boxed_local()
    }))
    .unwrap();
    pool.run_until_stalled();
    assert!(started.get());
    assert!(!closed.get());
    reference.release().unwrap();
    pool.run_until_stalled();
    assert!(
        !closed.get(),
        "source cleanup must retain the detached descendant"
    );
    let mut closing = Box::pin(host.close());
    pool.run_until(futures::future::poll_fn(|cx| {
        assert!(closing.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    assert!(!closed.get());
    send.send(()).unwrap();
    pool.run_until(closing).unwrap();
    assert!(closed.get());
    let diagnostics = host.kernel().diagnostics();
    assert_eq!(
        (
            diagnostics.roots,
            diagnostics.calls,
            diagnostics.objects,
            diagnostics.resources
        ),
        (0, 0, 0, 0)
    );
    assert!(host.errors().is_empty());
}

#[test]
fn a_foreign_contributor_cannot_claim_the_declaring_objects_observation() {
    use tembra::HookHandle;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let hook = Hook::<(), ()>::series("terminal");
    let declared = hook.clone();
    let bound = Rc::new(RefCell::new(None::<HookHandle<(), ()>>));
    let retained = bound.clone();
    let reasons = Rc::new(RefCell::new(Vec::new()));
    let captured = reasons.clone();
    host.install(vec![
        Orb::new("provider", 1).hook(hook.clone()).service(
            Service::new("provider", move |self_| {
                let handle = self_.hook(&declared)?;
                *retained.borrow_mut() = Some(handle.clone());
                Ok(handle)
            })
            .method("run", |hook, ctx, (): ()| async move {
                let source = ctx.with(&TRACE, "ordinary".into())?;
                hook.run(&source, ()).await?;
                hook.observe(&source, ()).await
            }),
        ),
        Orb::new("foreign", 1).requires(requirement()).contribute(
            "provider",
            &hook,
            0,
            move |ctx, ()| {
                let bound = bound.clone();
                let captured = captured.clone();
                async move {
                    let hook = bound.borrow().clone().unwrap();
                    let reason = hook.observe(&ctx, ()).await.unwrap_err().reason;
                    captured.borrow_mut().push(reason);
                    Ok(())
                }
            },
        ),
    ])
    .unwrap();
    let reference = Rc::new(host.get("provider", vec!["main".into()], "alice").unwrap());
    pool.run_until(host.root("alice", move |ctx| {
        async move { reference.call::<_, ()>(&ctx, "run", ()).await }.boxed_local()
    }))
    .unwrap();
    assert_eq!(
        reasons.borrow().as_slice(),
        ["operation_owner", "observation_source"]
    );
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().calls, 0);
    assert_eq!(host.kernel().diagnostics().roots, 0);
}
