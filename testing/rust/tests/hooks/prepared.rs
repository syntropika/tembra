mod support;
use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use support::*;
use tembra_core::{contracts::Contract, *};
use tembra_hooks::*;

fn declared<T: Clone + 'static>(
    h: &Harness,
    name: &str,
    build: impl Fn(&mut Declaration) -> Result<T> + 'static,
) -> (Reference, T) {
    let contract = Rc::new(Contract::local(name, 1, json!({}), BTreeMap::new()).unwrap());
    let saved = Rc::new(RefCell::new(None));
    let target = saved.clone();
    h.host
        .serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                *target.borrow_mut() = Some(build(declaration)?);
                Ok(Implementation::new())
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let reference = h
        .driver
        .run_until(h.host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    let value = saved.borrow_mut().take().unwrap();
    (reference, value)
}

#[test]
fn all_five_strategies_prepare_once_and_preserve_empty_behavior_and_then_forms() {
    let h = Harness::new();
    let manager = h.manager.clone();
    let calls = Rc::new(Cell::new(0));
    let count = calls.clone();
    let (
        reference,
        (series, bail, waterfall, middleware, parallel, series_then, waterfall_then, parallel_then),
    ) = declared(&h, "all-strategies", move |declaration| {
        let series = manager.declare_series(declaration, "duplicate", || {
            count.set(count.get() + 1);
            Ok(definition(
                "series",
                Strategy::Series,
                integer(),
                HookType::absent(),
                None,
            ))
        })?;
        let bail = manager.declare_bail(declaration, "duplicate", || {
            count.set(count.get() + 1);
            Ok(definition(
                "bail",
                Strategy::Bail,
                integer(),
                integer(),
                None,
            ))
        })?;
        let waterfall = manager.declare_waterfall(declaration, "duplicate", || {
            count.set(count.get() + 1);
            Ok(definition(
                "waterfall",
                Strategy::Waterfall,
                integer(),
                integer(),
                None,
            ))
        })?;
        let middleware = manager.declare_middleware(declaration, "duplicate", || {
            count.set(count.get() + 1);
            Ok(support::middleware("middleware"))
        })?;
        let parallel = manager.declare_parallel(declaration, "duplicate", || {
            count.set(count.get() + 1);
            Ok(definition(
                "parallel",
                Strategy::Parallel,
                integer(),
                integer(),
                None,
            ))
        })?;
        let terminal = Some(TerminalProfile {
            input: integer().profile().clone(),
            result: integer().profile().clone(),
        });
        let series_then = manager.declare_series(declaration, "duplicate", || {
            Ok(definition(
                "series-then",
                Strategy::Series,
                integer(),
                HookType::absent(),
                terminal.clone(),
            ))
        })?;
        let waterfall_then = manager.declare_waterfall(declaration, "duplicate", || {
            Ok(definition(
                "waterfall-then",
                Strategy::Waterfall,
                integer(),
                integer(),
                terminal.clone(),
            ))
        })?;
        let vector =
            HookType::<Vec<i32>>::serde(json!({"type":"array","items":{"type":"integer"}}))?;
        let parallel_then = manager.declare_parallel(declaration, "duplicate", || {
            Ok(definition(
                "parallel-then",
                Strategy::Parallel,
                integer(),
                integer(),
                Some(TerminalProfile {
                    input: vector.profile().clone(),
                    result: integer().profile().clone(),
                }),
            ))
        })?;
        Ok((
            series,
            bail,
            waterfall,
            middleware,
            parallel,
            series_then,
            waterfall_then,
            parallel_then,
        ))
    });
    assert_eq!(calls.get(), 5, "all producers run before the first turn");
    let terminal = h.terminal("all", integer(), integer(), identity());
    let vector_terminal = h.terminal(
        "vector",
        HookType::serde(json!({"type":"array","items":{"type":"integer"}})).unwrap(),
        integer(),
        method(|values: Vec<i32>, _| async move { Ok(values.len() as i32) }.boxed_local()),
    );
    h.driver
        .run_until(h.host.root(move |context| {
            async move {
                series.run(1, &context).await?;
                assert_eq!(bail.run(1, &context, None).await?, BailOutcome::Unresolved);
                assert_eq!(waterfall.run(7, &context).await?, 7);
                assert_eq!(middleware.run(8, &context, terminal.clone()).await?, 8);
                assert_eq!(parallel.run(9, &context).await?, Vec::<i32>::new());
                assert_eq!(
                    series_then.run_then(10, &context, terminal.clone()).await?,
                    10
                );
                assert_eq!(waterfall_then.run_then(11, &context, terminal).await?, 11);
                assert_eq!(
                    parallel_then
                        .run_then(12, &context, vector_terminal)
                        .await?,
                    0
                );
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(calls.get(), 5);
    reference.release();
    h.close();
}

#[test]
fn current_values_and_current_contributions_are_checked_on_each_run() {
    let h = Harness::new();
    let manager = h.manager.clone();
    let (reference, handle) = declared(&h, "live-plans", move |declaration| {
        manager.declare_waterfall(declaration, "live", || {
            let input = HookType::serde(json!({"type":"integer","minimum":0}))?;
            Ok(definition(
                "live",
                Strategy::Waterfall,
                input.clone(),
                input,
                None,
            ))
        })
    });
    let copy = handle.clone();
    assert_eq!(h.run(move |ctx| copy.run(4, &ctx)).unwrap(), 4);
    let copy = handle.clone();
    assert_eq!(
        h.run(move |ctx| copy.run(-1, &ctx)).unwrap_err().reason,
        "hook_value"
    );
    let input = HookType::serde(json!({"type":"integer","minimum":0})).unwrap();
    let def = definition("live", Strategy::Waterfall, input.clone(), input, None);
    h.add(
        &def,
        "late",
        0,
        method(|args: HookArgs<i32, i32>, _| async move { Ok(*args.input() + 1) }.boxed_local()),
    );
    h.ready();
    let copy = handle.clone();
    assert_eq!(h.run(move |ctx| copy.run(4, &ctx)).unwrap(), 5);
    let copy = handle.clone();
    let expired = h
        .driver
        .run_until(h.host.root(move |ctx| {
            async move {
                copy.run(0, &ctx).await?;
                Ok(ctx)
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(
        h.driver
            .run_until(handle.run(0, &expired))
            .unwrap_err()
            .reason,
        "expired"
    );
    reference.release();
    h.close();
}

#[test]
fn failed_or_panicking_producers_and_wrong_strategy_adopt_no_validator_captures() {
    let h = Harness::new();
    let manager = h.manager.clone();
    let weak = Rc::new(RefCell::new(Vec::new()));
    let captured = weak.clone();
    let (reference, handle) = declared(&h, "failures", move |declaration| {
        for mode in 0..3 {
            let marker = Rc::new(());
            captured.borrow_mut().push(Rc::downgrade(&marker));
            let error = manager
                .declare_waterfall(declaration, "retry", || {
                    let input = HookType::portable(json!({"type":"integer"}), move |v: &i32| {
                        let _ = &marker;
                        Ok(json!(v))
                    })?;
                    if mode == 0 {
                        return Err(RuntimeError::invalid("producer", "refused"));
                    }
                    if mode == 1 {
                        panic!("producer panic");
                    }
                    Ok(definition(
                        "wrong",
                        Strategy::Bail,
                        input.clone(),
                        input,
                        None,
                    ))
                })
                .err()
                .unwrap();
            assert_eq!(
                error.reason,
                ["producer", "hook_panic", "hook_strategy"][mode]
            );
            assert!(captured.borrow()[mode].upgrade().is_none());
        }
        manager.declare_waterfall(declaration, "retry", || {
            Ok(definition(
                "valid",
                Strategy::Waterfall,
                integer(),
                integer(),
                None,
            ))
        })
    });
    assert_eq!(weak.borrow().len(), 3);
    assert_eq!(h.run(move |ctx| handle.run(2, &ctx)).unwrap(), 2);
    reference.release();
    h.close();
}

#[test]
fn foreign_declaration_refuses_before_producer_and_disposal_expires_weak_handle() {
    let h = Harness::new();
    let other = Harness::new();
    let manager = h.manager.clone();
    let calls = Rc::new(Cell::new(0));
    let observed = calls.clone();
    let (foreign, ()) = declared(&other, "foreign", move |declaration| {
        let error = manager
            .declare_waterfall(declaration, "foreign", || {
                observed.set(observed.get() + 1);
                Ok(definition(
                    "never",
                    Strategy::Waterfall,
                    integer(),
                    integer(),
                    None,
                ))
            })
            .err()
            .unwrap();
        assert_eq!(error.reason, "implementation_owner");
        Ok(())
    });
    assert_eq!(calls.get(), 0);
    foreign.release();
    other.close();
    let manager = h.manager.clone();
    let (reference, handle) = declared(&h, "weak", move |decl| {
        manager.declare_waterfall(decl, "weak", || {
            Ok(definition(
                "weak",
                Strategy::Waterfall,
                integer(),
                integer(),
                None,
            ))
        })
    });
    reference.release();
    h.driver.run_until_stalled();
    let error = h.run(move |ctx| handle.run(1, &ctx)).unwrap_err();
    assert_eq!(error.code, ErrorCode::Gone);
    h.close();
}

#[test]
fn dynamic_portable_adapter_checks_actual_selected_operation_and_codecs_each_call() {
    use tembra_core::contracts::{Member, ValueSchema};
    let h = Harness::new();
    let manager = h.manager.clone();
    let (reference, handle) = declared(&h, "adapter", move |decl| {
        manager.declare_middleware(decl, "adapter", || Ok(middleware("adapter")))
    });
    let registry = Registry::new("terminals", ConflictPolicy::Error).unwrap();
    let view = h.host.registry(&registry).unwrap();
    let entries = Rc::new(Cell::new(0));
    for (key, delta, schema) in [
        ("first", 1, json!({"type":"integer"})),
        ("second", 2, json!({"type":"integer"})),
        ("wrong", 3, json!({"type":"string"})),
    ] {
        let token = Rc::new(
            Contract::local(
                key,
                1,
                json!({}),
                BTreeMap::from([(
                    "run".into(),
                    Member::Method {
                        args: Some(ValueSchema::Portable(schema)),
                        result: integer().profile().clone(),
                    },
                )]),
            )
            .unwrap(),
        );
        let registry = registry.clone();
        let entries = entries.clone();
        h.host
            .serve(
                token,
                Rc::new(move |_, _, decl| {
                    decl.register_method(&registry, key, json!(null), "run")?;
                    let native_entries = entries.clone();
                    let entries = entries.clone();
                    Ok(Implementation::new()
                        .method("run", move |value: i32, _| {
                            native_entries.set(native_entries.get() + 1);
                            async move { Ok(value + delta as i32) }.boxed_local()
                        })
                        .json_method("run", move |value, _| {
                            entries.set(entries.get() + 1);
                            async move { Ok(Some(json!(value.unwrap().as_i64().unwrap() + delta))) }
                                .boxed_local()
                        }))
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    h.ready();
    let codecs = Rc::new(Cell::new(0));
    let checks = codecs.clone();
    h.driver
        .run_until(h.host.root(move |ctx| {
            async move {
                for (key, expected) in [("first", 11), ("second", 12)] {
                    let current = view.table(Some(&ctx))?;
                    let operation = current.get(key).unwrap().operation().unwrap().clone();
                    let count = checks.clone();
                    assert_eq!(
                        handle
                            .run_portable(
                                10,
                                &ctx,
                                operation,
                                move |v| {
                                    count.set(count.get() + 1);
                                    Ok(Some(json!(v)))
                                },
                                |v| Ok(v.unwrap().as_i64().unwrap() as i32)
                            )
                            .await?,
                        expected
                    );
                }
                let current = view.table(Some(&ctx))?;
                let operation = current.get("first").unwrap().operation().unwrap().clone();
                assert_eq!(handle.run_native(20, &ctx, operation.clone()).await?, 21);
                let count = checks.clone();
                let error = handle
                    .run_portable(
                        10,
                        &ctx,
                        operation.clone(),
                        move |_| {
                            count.set(count.get() + 1);
                            Err(RuntimeError::invalid(
                                "encode_refused",
                                "input codec refused",
                            ))
                        },
                        |_| Ok(0),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.reason, "encode_refused");
                let count = checks.clone();
                let error = handle
                    .run_portable(
                        10,
                        &ctx,
                        operation,
                        move |v| {
                            count.set(count.get() + 1);
                            Ok(Some(json!(v)))
                        },
                        |_| {
                            Err(RuntimeError::invalid(
                                "decode_refused",
                                "output codec refused after terminal effect",
                            ))
                        },
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.reason, "decode_refused");
                let current = view.table(Some(&ctx))?;
                let operation = current.get("wrong").unwrap().operation().unwrap().clone();
                let count = checks.clone();
                assert_eq!(
                    handle
                        .run_portable(
                            10,
                            &ctx,
                            operation,
                            move |v| {
                                count.set(count.get() + 1);
                                Ok(Some(json!(v)))
                            },
                            |_: Option<Value>| Ok(0)
                        )
                        .await
                        .unwrap_err()
                        .reason,
                    "hook_signature"
                );
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(entries.get(), 4);
    assert_eq!(codecs.get(), 4);
    reference.release();
    h.close();
}

#[test]
fn prepared_observer_drop_delegates_cancellation_and_keeps_physical_run_budget() {
    let h = Harness::options(HookManagerOptions {
        max_active_runs: 1,
        ..Default::default()
    });
    let d = definition("budget", Strategy::Parallel, integer(), integer(), None);
    let manager = h.manager.clone();
    let prepared_definition = d.clone();
    let (reference, prepared) = declared(&h, "prepared-budget", move |decl| {
        manager.declare_parallel(decl, "budget", || Ok(prepared_definition.clone()))
    });
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
    let (handle, host) = (prepared.clone(), h.host.clone());
    h.driver.spawn(
        async move {
            host.root(move |ctx| {
                *saved.borrow_mut() = Some(ctx.clone());
                *target.borrow_mut() = Some(handle.run(1, &ctx));
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
    let handle = prepared.clone();
    assert_eq!(
        h.run(move |ctx| handle.run(1, &ctx)).unwrap_err().reason,
        "hook_runs"
    );
    assert_eq!(entries.get(), 1);
    finish.send(()).unwrap();
    h.driver.run_until_stalled();
    let handle = prepared.clone();
    assert_eq!(
        h.run(move |ctx| handle.run(1, &ctx)).unwrap_err().reason,
        "hook_runs"
    );
    drop(lease.borrow_mut().take());
    h.driver.run_until_stalled();
    let handle = prepared.clone();
    assert_eq!(h.run(move |ctx| handle.run(1, &ctx)).unwrap(), vec![2]);
    end.send(()).unwrap();
    h.driver.run_until_stalled();
    reference.release();
    h.close();
}
