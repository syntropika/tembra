mod support;
use futures::{
    FutureExt,
    future::{Either, LocalBoxFuture, select},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};
use support::*;
use tembra_core::*;
use tembra_hooks::*;

struct DropNextDriver {
    inner: Rc<ManualDriver>,
    discard: Cell<bool>,
}
impl Driver for DropNextDriver {
    fn spawn(&self, future: LocalBoxFuture<'static, ()>) {
        if self.discard.replace(false) {
            drop(future);
        } else {
            self.inner.spawn(future);
        }
    }
    fn now(&self) -> Duration {
        self.inner.now()
    }
    fn sleep(&self, duration: Duration) -> LocalBoxFuture<'static, ()> {
        self.inner.sleep(duration)
    }
}
#[test]
fn dropped_started_driver_task_settles_attempt_and_releases_run_capacity() {
    let driver = ManualDriver::new();
    let scheduler = Rc::new(DropNextDriver {
        inner: driver.clone(),
        discard: Cell::new(false),
    });
    let h = Harness::with_driver(
        driver,
        scheduler.clone(),
        HookManagerOptions {
            max_active_runs: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let d = middleware("dropped-task");
    h.add(
        &d,
        "outer",
        0,
        method(move |args: HookArgs<i32, i32>, _| {
            let scheduler = scheduler.clone();
            async move {
                scheduler.discard.set(true);
                let next = args.into_parts().1.unwrap();
                assert_eq!(
                    next.call(1).await.unwrap_err().reason,
                    "hook_next_abandoned"
                );
                next.recover(7)
            }
            .boxed_local()
        }),
    );
    let terminal = h.terminal("drop", integer(), integer(), identity());
    for _ in 0..3 {
        let (manager, definition, terminal) = (h.manager.clone(), d.clone(), terminal.clone());
        assert_eq!(
            h.run(move |ctx| manager.run_middleware(&definition, 1, &ctx, terminal))
                .unwrap(),
            7
        );
    }
    h.close();
}
#[test]
fn panicking_terminal_and_deeper_handler_return_typed_errors_without_stuck_started() {
    for deeper in [false, true] {
        let h = Harness::options(HookManagerOptions {
            max_active_runs: 1,
            ..Default::default()
        });
        let d = middleware("panics");
        h.add(
            &d,
            "outer",
            0,
            method(|args: HookArgs<i32, i32>, _| {
                async move { args.into_parts().1.unwrap().call(1).await }.boxed_local()
            }),
        );
        let panic_now = Rc::new(Cell::new(true));
        if deeper {
            let panic_now = panic_now.clone();
            h.add(
                &d,
                "deeper",
                1,
                method(move |args: HookArgs<i32, i32>, _| {
                    let panic_now = panic_now.clone();
                    async move {
                        assert!(!panic_now.get(), "deeper panic");
                        args.into_parts().1.unwrap().call(1).await
                    }
                    .boxed_local()
                }),
            );
        }
        let selected = panic_now.clone();
        let terminal = h.terminal(
            "panic",
            integer(),
            integer(),
            method(move |value, _| {
                let selected = selected.clone();
                async move {
                    assert!(deeper || !selected.get(), "terminal panic");
                    Ok(value)
                }
                .boxed_local()
            }),
        );
        let (manager, definition, target) = (h.manager.clone(), d.clone(), terminal.clone());
        assert_eq!(
            h.run(move |ctx| manager.run_middleware(&definition, 1, &ctx, target))
                .unwrap_err()
                .reason,
            "hook_panic"
        );
        panic_now.set(false);
        let manager = h.manager.clone();
        assert_eq!(
            h.run(move |ctx| manager.run_middleware(&d, 1, &ctx, terminal))
                .unwrap(),
            1
        );
        h.close();
    }
}
#[test]
fn panicking_deferred_decode_and_encode_are_recoverable_primary_failures() {
    for encode in [false, true] {
        let h = Harness::new();
        let d = middleware("codec-panic");
        h.add(
            &d,
            "outer",
            0,
            method(move |args: HookArgs<i32, i32>, _| {
                async move {
                    let next = args.into_parts().1.unwrap();
                    let (call, completion) = next
                        .call_with(
                            move || {
                                assert!(encode, "decode panic");
                                Ok(1)
                            },
                            move |value| {
                                assert!(!encode, "encode panic");
                                Ok(value)
                            },
                        )
                        .into_parts();
                    assert_eq!(call.await.unwrap_err().reason, "hook_panic");
                    completion.complete(Ok(()));
                    next.recover(5)
                }
                .boxed_local()
            }),
        );
        let terminal = h.terminal("codec", integer(), integer(), identity());
        let manager = h.manager.clone();
        assert_eq!(
            h.run(move |ctx| manager.run_middleware(&d, 1, &ctx, terminal))
                .unwrap(),
            5
        );
        h.close();
    }
}
#[test]
fn validator_panic_is_typed_before_handler_and_budget_admission() {
    let h = Harness::options(HookManagerOptions {
        max_active_runs: 1,
        ..Default::default()
    });
    let input = HookType::native("review.input@1", |value: &i32| {
        assert_ne!(*value, 1, "validator panic");
        Ok(())
    })
    .unwrap();
    let d = definition(
        "validator-panic",
        Strategy::Series,
        input,
        HookType::absent(),
        None,
    );
    let calls = Rc::new(Cell::new(0));
    let selected = calls.clone();
    h.add(
        &d,
        "handler",
        0,
        method(move |_: HookArgs<i32, ()>, _| {
            selected.set(selected.get() + 1);
            async { Ok(()) }.boxed_local()
        }),
    );
    h.ready();
    let (manager, definition) = (h.manager.clone(), d.clone());
    assert_eq!(
        h.run(move |ctx| manager.run_series(&definition, 1, &ctx))
            .unwrap_err()
            .reason,
        "hook_panic"
    );
    assert_eq!(calls.get(), 0);
    let manager = h.manager.clone();
    h.run(move |ctx| manager.run_series(&d, 2, &ctx)).unwrap();
    assert_eq!(calls.get(), 1);
    h.close();
}
#[test]
fn sequential_steps_wait_for_actual_detached_physical_entry() {
    let h = Harness::new();
    let d = definition(
        "physical-series",
        Strategy::Series,
        integer(),
        HookType::absent(),
        None,
    );
    let lease = Rc::new(RefCell::new(None));
    let lease_saved = lease.clone();
    let entered = Rc::new(RefCell::new(None));
    let entered_sender = entered.clone();
    let (send, receive) = futures::channel::oneshot::channel();
    *entered.borrow_mut() = Some(send);
    h.add_owned(&d, "first", move |owner| {
        let (lease_saved, entered_sender) = (lease_saved.clone(), entered_sender.clone());
        method(move |_: HookArgs<i32, ()>, ctx| {
            *lease_saved.borrow_mut() = Some(owner.retain(&ctx).unwrap());
            entered_sender
                .borrow_mut()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            async { Ok(()) }.boxed_local()
        })
    });
    let later = Rc::new(Cell::new(false));
    let selected = later.clone();
    h.add(
        &d,
        "second",
        1,
        method(move |_: HookArgs<i32, ()>, _| {
            selected.set(true);
            async { Ok(()) }.boxed_local()
        }),
    );
    h.ready();
    let manager = h.manager.clone();
    let run = h
        .host
        .root(move |ctx| manager.run_series(&d, 1, &ctx).boxed_local())
        .boxed_local();
    let Either::Right((_, run)) = h.driver.run_until(select(run, receive)) else {
        panic!("physical lease must keep series pending")
    };
    assert!(!later.get());
    drop(lease.borrow_mut().take());
    h.driver.run_until(run).unwrap();
    assert!(later.get());
    h.close();
}

#[test]
fn backend_admitted_attempt_cancelled_before_first_poll_still_settles() {
    let h = Harness::new();
    let d = middleware("cancel-before-poll");
    h.add(
        &d,
        "outer",
        0,
        method(|args: HookArgs<i32, i32>, context| {
            async move {
                let next = args.into_parts().1.unwrap();
                let (future, mut completion) = next.call_with(|| Ok(1), Ok).into_parts();
                completion.admit()?;
                context.cancel(RuntimeError::aborted(
                    "before_poll",
                    "cancelled before queued work polled",
                ));
                assert_eq!(future.await.unwrap_err().reason, "before_poll");
                completion.complete(Ok(()));
                Err(RuntimeError::aborted(
                    "before_poll",
                    "cancelled before queued work polled",
                ))
            }
            .boxed_local()
        }),
    );
    let terminal = h.terminal("before-poll", integer(), integer(), identity());
    let manager = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| manager.run_middleware(&d, 1, &ctx, terminal))
            .unwrap_err()
            .reason,
        "before_poll"
    );
    h.close();
}

#[test]
fn contained_handler_panic_still_joins_its_detached_physical_receipt() {
    let h = Harness::new();
    let d = middleware("panic-physical");
    h.add(
        &d,
        "outer",
        0,
        method(|args: HookArgs<i32, i32>, _| {
            async move { args.into_parts().1.unwrap().call(1).await }.boxed_local()
        }),
    );
    let saved_lease = Rc::new(RefCell::new(None));
    let lease = saved_lease.clone();
    let (send, entered) = futures::channel::oneshot::channel();
    let entered_sender = Rc::new(RefCell::new(Some(send)));
    h.add_owned(&d, "panicking", move |owner| {
        let (lease, entered_sender) = (lease.clone(), entered_sender.clone());
        method(move |_: HookArgs<i32, i32>, context| {
            *lease.borrow_mut() = Some(owner.retain(&context).unwrap());
            entered_sender
                .borrow_mut()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            async { panic!("handler panic with physical work still admitted") }.boxed_local()
        })
    });
    let terminal = h.terminal("panic-physical", integer(), integer(), identity());
    let manager = h.manager.clone();
    let run = h
        .host
        .root(move |context| {
            manager
                .run_middleware(&d, 1, &context, terminal)
                .boxed_local()
        })
        .boxed_local();
    let Either::Right((_, run)) = h.driver.run_until(select(run, entered)) else {
        panic!("panic does not prove independently retained physical work ended");
    };
    drop(saved_lease.borrow_mut().take());
    assert_eq!(h.driver.run_until(run).unwrap_err().reason, "hook_panic");
    h.close();
}

#[test]
fn receiver_self_cancel_refuses_native_next_without_cancelling_ancestor_root() {
    let h = Harness::new();
    let definition = middleware("self-cancel");
    h.add(
        &definition,
        "self-cancel",
        0,
        method(|args: HookArgs<i32, i32>, context| {
            async move {
                context.cancel(RuntimeError::aborted(
                    "self_cancel",
                    "receiver cancelled its own scope",
                ));
                let error = args.into_parts().1.unwrap().call(1).await.unwrap_err();
                assert_eq!(error.code, ErrorCode::Aborted);
                assert_eq!(error.reason, "self_cancel");
                Err(error)
            }
            .boxed_local()
        }),
    );
    let calls = Rc::new(Cell::new(0));
    let observed = calls.clone();
    let terminal = h.terminal(
        "self-cancel",
        integer(),
        integer(),
        method(move |value, _| {
            observed.set(observed.get() + 1);
            async move { Ok(value) }.boxed_local()
        }),
    );
    let after = middleware("after-cancel");
    let after_terminal = h.terminal("after-cancel", integer(), integer(), identity());
    let manager = h.manager.clone();
    h.driver
        .run_until(h.host.root(move |root| {
            async move {
                let error = manager
                    .run_middleware(&definition, 1, &root, terminal)
                    .await
                    .unwrap_err();
                assert_eq!(error.code, ErrorCode::Aborted);
                assert_eq!(error.reason, "self_cancel");
                root.check(root.host_id())?;
                assert_eq!(
                    manager
                        .run_middleware(&after, 7, &root, after_terminal)
                        .await?,
                    7
                );
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(calls.get(), 0);
    h.close();
}

#[test]
fn swallowed_admitted_pre_poll_cancellation_preserves_the_actual_primary_reason() {
    let h = Harness::new();
    let d = middleware("swallowed-pre-poll");
    h.add(
        &d,
        "swallowing-outer",
        0,
        method(|args: HookArgs<i32, i32>, context| {
            async move {
                let next = args.into_parts().1.unwrap();
                let (future, mut completion) = next.call_with(|| Ok(1), Ok).into_parts();
                completion.admit()?;
                context.cancel(RuntimeError::aborted(
                    "pre_poll_cause",
                    "receiver cancelled before poll",
                ));
                assert_eq!(future.await.unwrap_err().reason, "pre_poll_cause");
                completion.complete(Ok(()));
                Ok(3)
            }
            .boxed_local()
        }),
    );
    let terminal = h.terminal("swallowed-pre-poll", integer(), integer(), identity());
    let manager = h.manager.clone();
    let error = h
        .run(move |context| manager.run_middleware(&d, 1, &context, terminal))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Aborted);
    assert_eq!(error.reason, "pre_poll_cause");
    h.close();
}
