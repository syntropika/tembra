mod support;
use futures::{FutureExt, future::LocalBoxFuture};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use support::*;
use tembra_core::*;
use tembra_hooks::*;

#[test]
fn first_invalid_decode_consumes_next_and_explicit_recovery_prepares_before_ack() {
    let h = Harness::new();
    let d = middleware("deferred");
    let decoded = Rc::new(Cell::new(false));
    let seen = decoded.clone();
    h.add(
        &d,
        "outer",
        0,
        method(move |args: HookArgs<i32, i32>, _| {
            let seen = seen.clone();
            async move {
                let next = args.into_parts().1.unwrap();
                let selected = seen.clone();
                let (call, completion) = next
                    .call_with(
                        move || {
                            selected.set(true);
                            Err(RuntimeError::invalid("decode", "invalid raw input"))
                        },
                        Ok::<_, RuntimeError>,
                    )
                    .into_parts();
                assert!(!seen.get(), "decoder must be deferred until polling");
                assert_eq!(next.call(7).await.unwrap_err().reason, "hook_next_used");
                assert_eq!(call.await.unwrap_err().reason, "decode");
                completion.complete(Ok(()));
                let (invalid, projection) = next
                    .recover_with(
                        || Ok(3),
                        |_| {
                            Err::<i32, _>(RuntimeError::invalid("encode", "fallback encode failed"))
                        },
                    )
                    .into_parts();
                assert_eq!(invalid.await.unwrap_err().reason, "encode");
                projection.complete(Ok(()));
                next.recover(9)
            }
            .boxed_local()
        }),
    );
    let calls = Rc::new(Cell::new(0));
    let count = calls.clone();
    let terminal = h.terminal(
        "deferred",
        integer(),
        integer(),
        method(move |v, _| {
            count.set(count.get() + 1);
            async move { Ok(v) }.boxed_local()
        }),
    );
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_middleware(&d, 1, &ctx, terminal))
            .unwrap(),
        9
    );
    assert!(decoded.get());
    assert_eq!(calls.get(), 0);
    h.close();
}

#[test]
fn projection_errors_are_sticky_but_prepared_rejections_are_recoverable() {
    for sticky in [false, true] {
        let h = Harness::new();
        let d = middleware("projection");
        h.add(
            &d,
            "outer",
            0,
            method(move |args: HookArgs<i32, i32>, _| {
                async move {
                    let next = args.into_parts().1.unwrap();
                    let (call, completion) = next.call_with(|| Ok(1), Ok).into_parts();
                    assert_eq!(call.await.unwrap_err().reason, "downstream");
                    completion.complete(if sticky {
                        Err(RuntimeError::invalid("projection", "mint failed"))
                    } else {
                        Ok(())
                    });
                    next.recover(4)
                }
                .boxed_local()
            }),
        );
        let terminal = h.terminal(
            "projection",
            integer(),
            integer(),
            method(|_, _| {
                async { Err(RuntimeError::invalid("downstream", "failed")) }.boxed_local()
            }),
        );
        let m = h.manager.clone();
        let result = h.run(move |ctx| m.run_middleware(&d, 1, &ctx, terminal));
        if sticky {
            assert_eq!(result.unwrap_err().reason, "projection");
        } else {
            assert_eq!(result.unwrap(), 4);
        }
        h.close();
    }
}

#[test]
fn encoded_next_error_can_be_recovered_without_joining_its_own_projection_guard() {
    let h = Harness::new();
    let d = middleware("encoding");
    h.add(
        &d,
        "outer",
        0,
        method(|args: HookArgs<i32, i32>, _| {
            async move {
                let next = args.into_parts().1.unwrap();
                let (call, completion) = next
                    .call_with(
                        || Ok(1),
                        |_| Err::<i32, _>(RuntimeError::invalid("encode", "result codec failed")),
                    )
                    .into_parts();
                assert_eq!(call.await.unwrap_err().reason, "encode");
                completion.complete(Ok(()));
                next.recover(5)
            }
            .boxed_local()
        }),
    );
    let terminal = h.terminal("encoding", integer(), integer(), identity());
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_middleware(&d, 1, &ctx, terminal))
            .unwrap(),
        5
    );
    h.close();
}

#[test]
fn retained_unpolled_attempt_seals_without_downstream_and_late_next_expires() {
    let h = Harness::new();
    let d = middleware("retained");
    let saved = Rc::new(RefCell::new(None::<LocalBoxFuture<'static, Result<i32>>>));
    let retained = Rc::new(RefCell::new(None::<Next<i32, i32>>));
    let (target, late) = (saved.clone(), retained.clone());
    h.add(
        &d,
        "outer",
        0,
        method(move |args: HookArgs<i32, i32>, _| {
            let next = args.into_parts().1.unwrap();
            *target.borrow_mut() = Some(next.call(3));
            *late.borrow_mut() = Some(next);
            async { Ok(8) }.boxed_local()
        }),
    );
    let calls = Rc::new(Cell::new(0));
    let count = calls.clone();
    let terminal = h.terminal(
        "retained",
        integer(),
        integer(),
        method(move |v, _| {
            count.set(count.get() + 1);
            async move { Ok(v) }.boxed_local()
        }),
    );
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_middleware(&d, 1, &ctx, terminal))
            .unwrap_err()
            .reason,
        "hook_next_abandoned"
    );
    assert_eq!(calls.get(), 0);
    assert_eq!(
        h.driver
            .run_until(saved.borrow_mut().take().unwrap())
            .unwrap_err()
            .reason,
        "expired"
    );
    assert_eq!(
        h.driver
            .run_until(retained.borrow().as_ref().unwrap().call(4))
            .unwrap_err()
            .reason,
        "expired"
    );
    h.close();
}

#[test]
fn unwrapped_contribution_cannot_receive_an_active_next() {
    let h = Harness::new();
    let d = middleware("unwrapped");
    h.add_raw(
        &d,
        "bad",
        0,
        method(|args: HookArgs<i32, i32>, _| {
            async move {
                assert_eq!(
                    args.next().unwrap().call(1).await.unwrap_err().reason,
                    "hook_next_scope"
                );
                Ok(2)
            }
            .boxed_local()
        }),
    );
    let terminal = h.terminal("unwrapped", integer(), integer(), identity());
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_middleware(&d, 1, &ctx, terminal))
            .unwrap_err()
            .reason,
        "hook_signature"
    );
    h.close();
}

#[test]
fn fallback_validation_reentry_is_refused_and_invalid_fallback_does_not_consume_ack() {
    let h = Harness::new();
    let current = Rc::new(RefCell::new(None::<Next<i32, i32>>));
    let saved = current.clone();
    let enabled = Rc::new(Cell::new(false));
    let checking = enabled.clone();
    let result = HookType::<i32>::native("checked@1", move |value| {
        if checking.replace(false) {
            let next = saved.borrow().as_ref().cloned().unwrap();
            assert_eq!(next.recover(7).unwrap_err().reason, "hook_reentrancy");
        }
        if *value < 0 {
            return Err(RuntimeError::invalid("fallback", "negative fallback"));
        }
        Ok(())
    })
    .unwrap();
    let d = definition(
        "validation",
        Strategy::Middleware,
        integer(),
        result.clone(),
        Some(TerminalProfile {
            input: integer().profile().clone(),
            result: result.profile().clone(),
        }),
    );
    h.add(
        &d,
        "outer",
        0,
        method(move |args: HookArgs<i32, i32>, _| {
            let (current, enabled) = (current.clone(), enabled.clone());
            async move {
                let next = args.into_parts().1.unwrap();
                *current.borrow_mut() = Some(next.clone());
                assert_eq!(next.call(1).await.unwrap_err().reason, "failed");
                assert_eq!(next.recover(-1).unwrap_err().reason, "fallback");
                let (recovery, projection) = next
                    .recover_with(
                        move || {
                            enabled.set(true);
                            Ok(8)
                        },
                        Ok,
                    )
                    .into_parts();
                let value = recovery.await?;
                projection.complete(Ok(()));
                assert_eq!(next.recover(9).unwrap_err().reason, "hook_recovery");
                Ok(value)
            }
            .boxed_local()
        }),
    );
    let terminal = h.terminal(
        "validation",
        integer(),
        result,
        method(|_, _| async { Err(RuntimeError::invalid("failed", "failed")) }.boxed_local()),
    );
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_middleware(&d, 1, &ctx, terminal))
            .unwrap(),
        8
    );
    h.close();
}

#[test]
fn duplicate_refusal_is_ordinary_but_its_actual_output_projection_failure_is_sticky() {
    for sticky in [false, true] {
        let h = Harness::new();
        let d = middleware("duplicate");
        h.add(
            &d,
            "outer",
            0,
            method(move |args: HookArgs<i32, i32>, _| {
                async move {
                    let next = args.into_parts().1.unwrap();
                    let value = next.call(1).await?;
                    let (refused, projection) = next.call_with(|| Ok(2), Ok).into_parts();
                    assert_eq!(refused.await.unwrap_err().reason, "hook_next_used");
                    projection.complete(if sticky {
                        Err(RuntimeError::invalid("projection", "failed refusal mint"))
                    } else {
                        Ok(())
                    });
                    Ok(value)
                }
                .boxed_local()
            }),
        );
        let terminal = h.terminal("duplicate", integer(), integer(), identity());
        let m = h.manager.clone();
        let result = h.run(move |ctx| m.run_middleware(&d, 1, &ctx, terminal));
        if sticky {
            assert_eq!(result.unwrap_err().reason, "projection");
        } else {
            assert_eq!(result.unwrap(), 1);
        }
        h.close();
    }
}
