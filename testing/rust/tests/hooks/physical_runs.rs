mod support;
use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use support::*;
use tembra_core::*;
use tembra_hooks::*;

#[test]
fn first_parallel_logical_failure_cancels_siblings_before_detached_physical_lease_settles() {
    let h = Harness::new();
    let d = definition("physical", Strategy::Parallel, integer(), integer(), None);
    let (fail, failed) = oneshot::channel::<()>();
    let failed = Rc::new(RefCell::new(Some(failed)));
    let leases = Rc::new(RefCell::new(Vec::new()));
    let saved = leases.clone();
    h.add_owned(&d, "a", move |owner| {
        let (failed, saved) = (failed.clone(), saved.clone());
        method(move |_: HookArgs<i32, i32>, ctx| {
            saved.borrow_mut().push(owner.retain(&ctx).unwrap());
            let failed = failed.borrow_mut().take().unwrap();
            async move {
                failed.await.unwrap();
                Err(RuntimeError::invalid("first", "first failure"))
            }
            .boxed_local()
        })
    });
    let (finish, pending) = oneshot::channel::<()>();
    let pending = Rc::new(RefCell::new(Some(pending)));
    let canceled = Rc::new(Cell::new(false));
    let signal = canceled.clone();
    let started = Rc::new(Cell::new(false));
    let entered = started.clone();
    h.add(
        &d,
        "b",
        0,
        method(move |_: HookArgs<i32, i32>, ctx| {
            entered.set(true);
            let (pending, signal) = (pending.borrow_mut().take().unwrap(), signal.clone());
            async move {
                ctx.cancellation().cancelled().await;
                signal.set(true);
                pending.await.unwrap();
                Ok(2)
            }
            .boxed_local()
        }),
    );
    h.ready();
    let (m, host) = (h.manager.clone(), h.host.clone());
    let output = Rc::new(RefCell::new(None));
    let result = output.clone();
    h.driver.spawn(
        async move {
            *result.borrow_mut() = Some(
                host.root(move |ctx| {
                    async move {
                        let outcome = m.run_dynamic_parallel(&d, 1, &ctx).await;
                        ctx.check(ctx.host_id())
                            .expect("sibling failure cannot cancel caller root");
                        outcome
                    }
                    .boxed_local()
                })
                .await,
            );
        }
        .boxed_local(),
    );
    h.driver.run_until_stalled();
    assert!(started.get());
    fail.send(()).unwrap();
    h.driver.run_until_stalled();
    assert!(
        canceled.get(),
        "logical failure must promptly signal started siblings"
    );
    assert!(
        output.borrow().is_none(),
        "physical lease and sibling work are still joined"
    );
    finish.send(()).unwrap();
    h.driver.run_until_stalled();
    assert!(output.borrow().is_none());
    leases.borrow_mut().clear();
    h.driver.run_until_stalled();
    assert_eq!(
        output.borrow_mut().take().unwrap().unwrap_err().reason,
        "first"
    );
    h.close();
}

#[test]
fn options_bounds_are_checked_before_any_run() {
    for options in [
        HookManagerOptions {
            max_contributions: 0,
            ..Default::default()
        },
        HookManagerOptions {
            max_contributions: 4097,
            ..Default::default()
        },
        HookManagerOptions {
            max_active_runs: 0,
            ..Default::default()
        },
        HookManagerOptions {
            max_active_runs: 1025,
            ..Default::default()
        },
    ] {
        assert_eq!(
            Harness::checked_options(options).err().unwrap().reason,
            "hook_options"
        );
    }
    let profile = HookType::<Rc<i32>>::native("rc@1", |_| Ok(())).unwrap();
    let absent = HookDescriptor::new(
        "wrong-unit",
        1,
        Strategy::Series,
        profile.profile().clone(),
        None,
        None,
    )
    .unwrap();
    assert!(
        RustHookDefinition::from_descriptor(
            absent,
            profile,
            HookType::<i32>::serde(json!({"type":"integer"})).unwrap()
        )
        .is_err()
    );
}

#[test]
fn dropped_observer_keeps_run_budget_until_started_work_and_detached_lease_settle() {
    let h = Harness::options(HookManagerOptions {
        max_active_runs: 1,
        ..Default::default()
    });
    let d = definition("budget", Strategy::Parallel, integer(), integer(), None);
    let (finish, pending) = oneshot::channel::<()>();
    let pending = Rc::new(RefCell::new(Some(pending)));
    let lease = Rc::new(RefCell::new(None));
    let selected = lease.clone();
    let canceled = Rc::new(Cell::new(false));
    let signal = canceled.clone();
    let entries = Rc::new(Cell::new(0));
    let count = entries.clone();
    h.add_owned(&d, "owner", move |owner| {
        let (pending, selected, signal, count) = (
            pending.clone(),
            selected.clone(),
            signal.clone(),
            count.clone(),
        );
        method(move |_: HookArgs<i32, i32>, ctx| {
            count.set(count.get() + 1);
            let Some(pending) = pending.borrow_mut().take() else {
                return async { Ok(2) }.boxed_local();
            };
            *selected.borrow_mut() = Some(owner.retain(&ctx).unwrap());
            let signal = signal.clone();
            async move {
                ctx.cancellation().cancelled().await;
                signal.set(true);
                pending.await.unwrap();
                Ok(1)
            }
            .boxed_local()
        })
    });
    h.ready();
    let (end, ended) = oneshot::channel::<()>();
    let run = Rc::new(RefCell::new(None::<HookRun<Vec<i32>>>));
    let target = run.clone();
    let caller = Rc::new(RefCell::new(None::<Context>));
    let saved = caller.clone();
    let (m, definition, host) = (h.manager.clone(), d.clone(), h.host.clone());
    h.driver.spawn(
        async move {
            host.root(move |ctx| {
                *saved.borrow_mut() = Some(ctx.clone());
                *target.borrow_mut() = Some(m.run_dynamic_parallel(&definition, 1, &ctx));
                async move {
                    ended.await.unwrap();
                    Ok(())
                }
                .boxed_local()
            })
            .await
            .unwrap();
        }
        .boxed_local(),
    );
    h.driver.run_until_stalled();
    let mut waiting = run.borrow_mut().take().unwrap();
    h.driver.run_until(async {
        assert!(futures::poll!(&mut waiting).is_pending());
    });
    *run.borrow_mut() = Some(waiting);
    h.driver.run_until_stalled();
    assert_eq!(entries.get(), 1);
    drop(run.borrow_mut().take());
    h.driver.run_until_stalled();
    assert!(canceled.get());
    caller
        .borrow()
        .as_ref()
        .unwrap()
        .check(h.host.id())
        .unwrap();
    let (m, definition) = (h.manager.clone(), d.clone());
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_parallel(&definition, 1, &ctx))
            .unwrap_err()
            .reason,
        "hook_runs"
    );
    assert_eq!(entries.get(), 1);
    finish.send(()).unwrap();
    h.driver.run_until_stalled();
    let (m, definition) = (h.manager.clone(), d.clone());
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_parallel(&definition, 1, &ctx))
            .unwrap_err()
            .reason,
        "hook_runs"
    );
    drop(lease.borrow_mut().take());
    h.driver.run_until_stalled();
    let m = h.manager.clone();
    assert_eq!(
        h.run(move |ctx| m.run_dynamic_parallel(&d, 1, &ctx))
            .unwrap(),
        vec![2]
    );
    end.send(()).unwrap();
    h.driver.run_until_stalled();
    h.close();
}

#[test]
fn complete_plan_type_preflight_and_contribution_limit_run_no_handlers() {
    for limit in [false, true] {
        let h = Harness::options(HookManagerOptions {
            max_contributions: if limit { 1 } else { 1024 },
            ..Default::default()
        });
        let d = definition(
            "preflight",
            Strategy::Series,
            integer(),
            HookType::absent(),
            None,
        );
        let calls = Rc::new(Cell::new(0));
        let count = calls.clone();
        h.add(
            &d,
            "a",
            0,
            method(move |_: HookArgs<i32, ()>, _| {
                count.set(count.get() + 1);
                async { Ok(()) }.boxed_local()
            }),
        );
        if limit {
            h.add(
                &d,
                "b",
                0,
                method(|_: HookArgs<i32, ()>, _| async { Ok(()) }.boxed_local()),
            );
        } else {
            h.add(
                &d,
                "b",
                0,
                method(|_: HookArgs<i32, i32>, _| async { Ok(1) }.boxed_local()),
            );
        }
        h.ready();
        let m = h.manager.clone();
        let error = h
            .run(move |ctx| m.run_dynamic_series(&d, 1, &ctx))
            .unwrap_err();
        assert_eq!(
            error.reason,
            if limit {
                "hook_contributions"
            } else {
                "native_type"
            }
        );
        assert_eq!(calls.get(), 0);
        h.close();
    }
}
