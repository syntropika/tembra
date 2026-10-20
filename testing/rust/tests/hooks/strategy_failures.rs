mod support;
use futures::{
    FutureExt,
    channel::oneshot,
    future::{Either, LocalBoxFuture, select},
};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use support::*;
use tembra_core::*;
use tembra_hooks::*;

type Dispatch = Box<dyn FnOnce(Context) -> LocalBoxFuture<'static, Result<()>>>;
fn failure<H: 'static>(
    context: Context,
    entered: Rc<RefCell<Option<oneshot::Sender<()>>>>,
    cancel: bool,
    fallback: Option<H>,
) -> LocalBoxFuture<'static, Result<H>> {
    async move {
        entered.borrow_mut().take().unwrap().send(()).unwrap();
        if cancel {
            let cause = context.cancellation().cancelled().await;
            fallback.ok_or(cause)
        } else {
            Err(RuntimeError::invalid("injected", "handler failed"))
        }
    }
    .boxed_local()
}
fn sequential_case(strategy: Strategy, cancel: bool, ignore_cancel: bool) {
    let h = Harness::new();
    let later = Rc::new(Cell::new(0));
    let terminal_calls = Rc::new(Cell::new(0));
    let (send, receive) = oneshot::channel();
    let entered = Rc::new(RefCell::new(Some(send)));
    let profile = Some(TerminalProfile {
        input: integer().profile().clone(),
        result: integer().profile().clone(),
    });
    let terminal_counter = terminal_calls.clone();
    let terminal = h.terminal(
        "end",
        integer(),
        integer(),
        method(move |value, _| {
            terminal_counter.set(terminal_counter.get() + 1);
            async move { Ok(value) }.boxed_local()
        }),
    );
    let dispatch: Dispatch = match strategy {
        Strategy::Series => {
            let d = definition(
                "series-errors",
                strategy,
                integer(),
                HookType::absent(),
                profile,
            );
            h.add(
                &d,
                "first",
                0,
                method(move |_: HookArgs<i32, ()>, ctx| {
                    failure(ctx, entered.clone(), cancel, ignore_cancel.then_some(()))
                }),
            );
            let later = later.clone();
            h.add(
                &d,
                "later",
                1,
                method(move |_: HookArgs<i32, ()>, _| {
                    later.set(later.get() + 1);
                    async { Ok(()) }.boxed_local()
                }),
            );
            let manager = h.manager.clone();
            Box::new(move |ctx| {
                manager
                    .run_series_then(&d, 1, &ctx, terminal)
                    .map(|result| result.map(|_| ()))
                    .boxed_local()
            })
        }
        Strategy::Bail => {
            let d = definition("bail-errors", strategy, integer(), integer(), profile);
            h.add(
                &d,
                "first",
                0,
                method(move |_: HookArgs<i32, BailDecision<i32>>, ctx| {
                    failure(
                        ctx,
                        entered.clone(),
                        cancel,
                        ignore_cancel.then_some(BailDecision::Continue),
                    )
                }),
            );
            let later = later.clone();
            h.add(
                &d,
                "later",
                1,
                method(move |_: HookArgs<i32, BailDecision<i32>>, _| {
                    later.set(later.get() + 1);
                    async { Ok(BailDecision::Continue) }.boxed_local()
                }),
            );
            let manager = h.manager.clone();
            Box::new(move |ctx| {
                manager
                    .run_bail(&d, 1, &ctx, Some(terminal))
                    .map(|result| result.map(|_| ()))
                    .boxed_local()
            })
        }
        Strategy::Waterfall => {
            let d = definition("waterfall-errors", strategy, integer(), integer(), profile);
            h.add(
                &d,
                "first",
                0,
                method(move |_: HookArgs<i32, i32>, ctx| {
                    failure(ctx, entered.clone(), cancel, ignore_cancel.then_some(1))
                }),
            );
            let later = later.clone();
            h.add(
                &d,
                "later",
                1,
                method(move |args: HookArgs<i32, i32>, _| {
                    later.set(later.get() + 1);
                    async move { Ok(args.into_parts().0) }.boxed_local()
                }),
            );
            let manager = h.manager.clone();
            Box::new(move |ctx| {
                manager
                    .run_waterfall_then(&d, 1, &ctx, terminal)
                    .map(|result| result.map(|_| ()))
                    .boxed_local()
            })
        }
        _ => unreachable!(),
    };
    h.ready();
    let token = Rc::new(RefCell::new(None));
    let selected = token.clone();
    let run = h
        .host
        .root(move |ctx| {
            *selected.borrow_mut() = Some(ctx.cancellation().clone());
            dispatch(ctx)
        })
        .boxed_local();
    let error = if cancel {
        let Either::Right((_, run)) = h.driver.run_until(select(run, receive)) else {
            panic!("handler must wait for caller cancellation")
        };
        token
            .borrow()
            .as_ref()
            .unwrap()
            .cancel(RuntimeError::aborted("injected_cancel", "caller cancelled"));
        h.driver.run_until(run).unwrap_err()
    } else {
        h.driver.run_until(run).unwrap_err()
    };
    assert_eq!(
        error.reason,
        if cancel {
            "injected_cancel"
        } else {
            "injected"
        }
    );
    assert_eq!(
        error.code,
        if cancel {
            ErrorCode::Aborted
        } else {
            ErrorCode::Invalid
        }
    );
    assert_eq!(later.get(), 0, "failure must not be treated as Continue");
    assert_eq!(terminal_calls.get(), 0);
    h.close();
}
#[test]
fn series_bail_and_waterfall_errors_stop_later_contributions_and_terminal() {
    for strategy in [Strategy::Series, Strategy::Bail, Strategy::Waterfall] {
        sequential_case(strategy, false, false);
    }
}
#[test]
fn series_bail_and_waterfall_caller_cancellation_stop_later_contributions_and_terminal() {
    for strategy in [Strategy::Series, Strategy::Bail, Strategy::Waterfall] {
        sequential_case(strategy, true, false);
    }
}
#[test]
fn invalid_bail_waterfall_and_parallel_handler_results_are_refused() {
    for strategy in [Strategy::Bail, Strategy::Waterfall, Strategy::Parallel] {
        let h = Harness::new();
        let limited = HookType::<i32>::serde(json!({"type":"integer","maximum":5})).unwrap();
        let later = Rc::new(Cell::new(0));
        let definition = definition("invalid-output", strategy, limited.clone(), limited, None);
        let error = match strategy {
            Strategy::Bail => {
                h.add(
                    &definition,
                    "invalid",
                    0,
                    method(|_: HookArgs<i32, BailDecision<i32>>, _| {
                        async { Ok(BailDecision::Stop(9)) }.boxed_local()
                    }),
                );
                let later = later.clone();
                h.add(
                    &definition,
                    "later",
                    1,
                    method(move |_: HookArgs<i32, BailDecision<i32>>, _| {
                        later.set(later.get() + 1);
                        async { Ok(BailDecision::Continue) }.boxed_local()
                    }),
                );
                h.ready();
                let manager = h.manager.clone();
                h.run(move |ctx| manager.run_bail(&definition, 1, &ctx, None))
                    .unwrap_err()
            }
            Strategy::Waterfall => {
                h.add(
                    &definition,
                    "invalid",
                    0,
                    method(|_: HookArgs<i32, i32>, _| async { Ok(9) }.boxed_local()),
                );
                let later = later.clone();
                h.add(
                    &definition,
                    "later",
                    1,
                    method(move |_: HookArgs<i32, i32>, _| {
                        later.set(later.get() + 1);
                        async { Ok(2) }.boxed_local()
                    }),
                );
                h.ready();
                let manager = h.manager.clone();
                h.run(move |ctx| manager.run_waterfall(&definition, 1, &ctx))
                    .unwrap_err()
            }
            Strategy::Parallel => {
                h.add(
                    &definition,
                    "invalid",
                    0,
                    method(|_: HookArgs<i32, i32>, _| async { Ok(9) }.boxed_local()),
                );
                h.ready();
                let manager = h.manager.clone();
                h.run(move |ctx| manager.run_parallel(&definition, 1, &ctx))
                    .unwrap_err()
            }
            _ => unreachable!(),
        };
        assert_eq!(error.code, ErrorCode::Invalid);
        assert_eq!(error.reason, "hook_value");
        assert_eq!(later.get(), 0);
        h.close();
    }
}

#[test]
fn invalid_terminal_result_is_reported_after_its_actual_effect() {
    let h = Harness::new();
    let limited = HookType::<i32>::serde(json!({"type":"integer","maximum":5})).unwrap();
    let d = definition(
        "terminal-invalid",
        Strategy::Series,
        integer(),
        HookType::absent(),
        Some(TerminalProfile {
            input: integer().profile().clone(),
            result: limited.profile().clone(),
        }),
    );
    let effects = Rc::new(Cell::new(0));
    let selected = effects.clone();
    let terminal = h.terminal(
        "invalid",
        integer(),
        limited,
        method(move |_: i32, _| {
            selected.set(selected.get() + 1);
            async { Ok(9) }.boxed_local()
        }),
    );
    let manager = h.manager.clone();
    let error = h
        .run(move |context| manager.run_series_then(&d, 1, &context, terminal))
        .unwrap_err();
    assert_eq!(error.reason, "hook_value");
    assert_eq!(
        effects.get(),
        1,
        "result refusal does not roll back terminal effects"
    );
    h.close();
}

#[test]
fn manager_revalidates_result_changed_after_the_checked_wrapper_returns() {
    let h = Harness::new();
    let limited = HookType::<i32>::serde(json!({"type":"integer","maximum":5})).unwrap();
    let d = definition(
        "outer-mutated-output",
        Strategy::Bail,
        integer(),
        limited,
        None,
    );
    let checked = hook_handler(method(|_: HookArgs<i32, BailDecision<i32>>, _| {
        async { Ok(BailDecision::Stop(4)) }.boxed_local()
    }));
    h.add_raw(
        &d,
        "outer-mutator",
        0,
        method(move |args, context| {
            let checked = checked.clone();
            async move { checked(args, context).await.map(|_| BailDecision::Stop(9)) }.boxed_local()
        }),
    );
    h.ready();
    let manager = h.manager.clone();
    let error = h
        .run(move |context| manager.run_bail(&d, 1, &context, None))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Invalid);
    assert_eq!(error.reason, "hook_value");
    h.close();
}

#[test]
fn sequential_manager_checks_cancellation_even_when_handler_returns_success() {
    for strategy in [Strategy::Series, Strategy::Bail, Strategy::Waterfall] {
        sequential_case(strategy, true, true);
    }
}
