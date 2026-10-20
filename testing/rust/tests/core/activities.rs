use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use std::{
    cell::{Cell as Flag, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{contracts::Contract, *};
fn token(name: &str) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), BTreeMap::new()).unwrap())
}

#[test]
fn state_and_typed_event_listeners_enter_actual_roots_and_detach_on_close() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("activities", json!("principal")),
        driver.clone(),
    );
    let contract = token("listeners");
    let sink = Rc::new(RefCell::new(None::<Rc<dyn Fn(u32)>>));
    let detached = Rc::new(Flag::new(0));
    let detach = detached.clone();
    let emit = sink.clone();
    let source = EventSource::new(move |callback| {
        *emit.borrow_mut() = Some(callback);
        let emit = emit.clone();
        let detach = detach.clone();
        Ok(Box::new(move || {
            emit.borrow_mut().take();
            detach.set(detach.get() + 1);
        }))
    });
    let seen = Rc::new(RefCell::new(Vec::new()));
    let saved = Rc::new(RefCell::new(None));
    let changes = saved.clone();
    let events = seen.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let state = declaration.state(
                "number",
                StateDeclaration::new(json!({"type":"integer"}), json!(2)),
            )?;
            *changes.borrow_mut() = Some(state.clone());
            let output = events.clone();
            declaration.on("state", state.readable(), move |snapshot, context| {
                context.check(context.host_id()).unwrap();
                assert_eq!(context.code_version(), 1);
                let output = output.clone();
                async move {
                    output
                        .borrow_mut()
                        .push(snapshot.value().as_u64().unwrap() as u32);
                    Ok(())
                }
                .boxed_local()
            })?;
            let output = events.clone();
            declaration.on_event("event", source.clone(), move |event, context| {
                context.check(context.host_id()).unwrap();
                let output = output.clone();
                async move {
                    output.borrow_mut().push(event);
                    Ok(())
                }
                .boxed_local()
            })?;
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(&*seen.borrow(), &[2]);
    let callback = sink.borrow().clone().unwrap();
    callback(7);
    driver.run_until_stalled();
    let state = saved.borrow().clone().unwrap();
    driver
        .run_until(host.root(move |context| {
            async move {
                state.replace(&context, json!(3), ReplaceOptions::default())?;
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(&*seen.borrow(), &[2, 7, 3]);
    reference.release();
    driver.run_until(host.close()).unwrap();
    assert_eq!(detached.get(), 1);
    callback(9);
    driver.run_until_stalled();
    assert_eq!(&*seen.borrow(), &[2, 7, 3]);
}
#[test]
fn detached_task_root_shares_physical_entry_after_task_settles_and_keeps_resource_open() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("task-physical", json!(null)),
        driver.clone(),
    );
    let contract = token("task");
    let (tx, rx) = oneshot::channel::<()>();
    let rx = Rc::new(RefCell::new(Some(rx)));
    let closes = Rc::new(Flag::new(0));
    let task_done = Rc::new(Flag::new(false));
    let root_started = Rc::new(Flag::new(false));
    let root_done = Rc::new(Flag::new(false));
    let saved = Rc::new(RefCell::new(None));
    let (tasks, closed, finished, entered, completed, scopes) = (
        driver.clone(),
        closes.clone(),
        task_done.clone(),
        root_started.clone(),
        root_done.clone(),
        saved.clone(),
    );
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let closed = closed.clone();
            let resource = declaration.resource(
                "value",
                ResourceDeclaration::new(|_| async { Ok(41_u32) }.boxed_local()).with_close(
                    move |_| {
                        closed.set(closed.get() + 1);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            let (rx, tasks, finished, entered, completed, scopes) = (
                rx.clone(),
                tasks.clone(),
                finished.clone(),
                entered.clone(),
                completed.clone(),
                scopes.clone(),
            );
            declaration.task(move |scope| {
                *scopes.borrow_mut() = Some(scope.clone());
                let gate = rx.borrow_mut().take().unwrap();
                let resource = resource.clone();
                let entered = entered.clone();
                let completed = completed.clone();
                let body = scope.root(move |_context| {
                    async move {
                        entered.set(true);
                        gate.await.unwrap();
                        assert_eq!(*resource.value()?, 41);
                        completed.set(true);
                        Ok(())
                    }
                    .boxed_local()
                });
                tasks.spawn(
                    async move {
                        body.await.unwrap();
                    }
                    .boxed_local(),
                );
                finished.set(true);
                async { Ok(()) }.boxed_local()
            })?;
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    driver.run_until_stalled();
    assert!(task_done.get());
    assert!(root_started.get());
    assert!(saved.borrow().as_ref().unwrap().retain().is_err());
    reference.release();
    let closed = Rc::new(Flag::new(false));
    let completed = closed.clone();
    let closing = host.clone();
    driver.spawn(
        async move {
            closing.close().await.unwrap();
            completed.set(true);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(!closed.get());
    assert_eq!(closes.get(), 0);
    assert!(!root_done.get());
    tx.send(()).unwrap();
    driver.run_until_stalled();
    assert!(root_done.get());
    assert!(closed.get());
    assert_eq!(closes.get(), 1);
}
#[test]
fn event_source_weak_identity_and_clone_local_owners_release_without_cycles() {
    struct Owner(Rc<Flag<usize>>);
    impl Drop for Owner {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let drops = Rc::new(Flag::new(0));
    let source = EventSource::<u32>::new(|_| Ok(Box::new(|| {})));
    let anchored = source.clone().retain_owner(Rc::new(Owner(drops.clone())));
    let weak = anchored.downgrade();
    let clone = weak.upgrade().unwrap();
    drop(anchored);
    assert_eq!(drops.get(), 0);
    drop(clone);
    assert_eq!(drops.get(), 1);
    assert!(weak.upgrade().is_none());
    drop(source);
    assert_eq!(drops.get(), 1);
    assert!(ListenerOptions::new(0).is_err());
    assert!(ListenerOptions::new(1025).is_err());
}

#[test]
fn resource_selected_source_owner_lives_through_actual_detach() {
    struct Anchor(Rc<Flag<usize>>);
    impl Drop for Anchor {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("selected-source", json!(null)),
        driver.clone(),
    );
    let contract = token("selected");
    let drops = Rc::new(Flag::new(0));
    let detached = Rc::new(Flag::new(0));
    let count = drops.clone();
    let detach = detached.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let resource = declaration.resource(
                "value",
                ResourceDeclaration::new(|_| async { Ok(1_u32) }.boxed_local()),
            )?;
            let count = count.clone();
            let detach = detach.clone();
            declaration.on_resource(
                "selected",
                resource,
                Rc::new(move |_| {
                    let detach = detach.clone();
                    Ok(EventSource::<u32>::new(move |_| {
                        let detach = detach.clone();
                        Ok(Box::new(move || detach.set(detach.get() + 1)))
                    })
                    .retain_owner(Rc::new(Anchor(count.clone()))))
                }),
                |_, _| async { Ok(()) }.boxed_local(),
            )?;
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(drops.get(), 0);
    assert_eq!(detached.get(), 0);
    reference.release();
    driver.run_until(host.close()).unwrap();
    assert_eq!(detached.get(), 1);
    assert_eq!(drops.get(), 1);
}

#[test]
fn actual_reload_restarts_replayed_task_and_keeps_unchanged_alias_task_running() {
    fn facet(contract: Rc<Contract>, version: u32, seen: Rc<RefCell<Vec<u32>>>) -> ExecutableFacet {
        ExecutableFacet::foreign("task-version", Vec::new(), move |_, declarations| {
            let seen = seen.clone();
            declarations.serve(
                contract.clone(),
                Rc::new(move |_, _, declaration| {
                    let seen = seen.clone();
                    declaration.task(move |scope| {
                        let seen = seen.clone();
                        async move {
                            scope
                                .root(move |context| {
                                    async move {
                                        assert_eq!(context.code_version(), u64::from(version));
                                        seen.borrow_mut().push(version);
                                        Ok(())
                                    }
                                    .boxed_local()
                                })
                                .await?;
                            scope.signal().cancelled().await;
                            Ok(())
                        }
                        .boxed_local()
                    })?;
                    Ok(Implementation::new())
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..ServeOptions::default()
                },
            )
        })
        .unwrap()
    }
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("task-reload", json!(null)), driver.clone());
    let contract = token("reload-task");
    let seen = Rc::new(RefCell::new(Vec::new()));
    driver
        .run_until(
            host.install_executable(
                facet(contract.clone(), 1, seen.clone()),
                LoadOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(&*seen.borrow(), &[1]);
    driver
        .run_until(host.reload(
            vec![facet(contract, 2, seen.clone())],
            ReloadOptions::default(),
        ))
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(&*seen.borrow(), &[1, 2]);
    driver
        .run_until(host.reload(vec![], ReloadOptions::default()))
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(&*seen.borrow(), &[1, 2]);
    driver.run_until(host.close()).unwrap();
    assert!(host.errors().is_empty());
}

#[test]
fn aborted_gate_defers_old_task_restart_until_detached_same_entry_physically_settles() {
    fn facet(
        contract: Rc<Contract>,
        version: u64,
        starts: Rc<Flag<usize>>,
        receiver: Rc<RefCell<Option<oneshot::Receiver<()>>>>,
        driver: Rc<ManualDriver>,
        saved: Rc<RefCell<Option<Cell>>>,
    ) -> ExecutableFacet {
        ExecutableFacet::foreign("task-gate", vec![], move |_, declarations| {
            let (starts, receiver, driver, saved) = (
                starts.clone(),
                receiver.clone(),
                driver.clone(),
                saved.clone(),
            );
            declarations.serve(
                contract.clone(),
                Rc::new(move |_, _, declaration| {
                    let state = declaration.state(
                        "value",
                        StateDeclaration::new(json!({"type":"integer"}), json!(7)).version(version),
                    )?;
                    *saved.borrow_mut() = Some(state.clone());
                    let (starts, receiver, driver) =
                        (starts.clone(), receiver.clone(), driver.clone());
                    declaration.task(move |scope| {
                        starts.set(starts.get() + 1);
                        let pending = receiver.borrow_mut().take();
                        if let Some(gate) = pending {
                            let body = scope.root(move |_| {
                                async move {
                                    gate.await.unwrap();
                                    Ok(())
                                }
                                .boxed_local()
                            });
                            driver.spawn(
                                async move {
                                    body.await.unwrap();
                                }
                                .boxed_local(),
                            );
                            async { Ok(()) }.boxed_local()
                        } else {
                            async move {
                                scope.signal().cancelled().await;
                                Ok(())
                            }
                            .boxed_local()
                        }
                    })?;
                    Ok(Implementation::new())
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..ServeOptions::default()
                },
            )
        })
        .unwrap()
    }
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("task-gate", json!(null)), driver.clone());
    let contract = token("gate-owner");
    let (old_starts, new_starts) = (Rc::new(Flag::new(0)), Rc::new(Flag::new(0)));
    let (tx, rx) = oneshot::channel();
    let saved = Rc::new(RefCell::new(None));
    driver
        .run_until(
            host.install_executable(
                facet(
                    contract.clone(),
                    1,
                    old_starts.clone(),
                    Rc::new(RefCell::new(Some(rx))),
                    driver.clone(),
                    saved.clone(),
                ),
                LoadOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(old_starts.get(), 1);
    let state = saved.borrow().clone().unwrap();
    let before = state.snapshot().unwrap();
    let result = Rc::new(RefCell::new(None));
    let received = result.clone();
    let reloading = host.clone();
    let candidate = facet(
        contract,
        2,
        new_starts.clone(),
        Rc::new(RefCell::new(None)),
        driver.clone(),
        saved,
    );
    driver.spawn(
        async move {
            *received.borrow_mut() = Some(
                reloading
                    .reload(
                        vec![candidate],
                        ReloadOptions {
                            gate_deadline: std::time::Duration::from_millis(3),
                            ..ReloadOptions::default()
                        },
                    )
                    .await,
            );
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(result.borrow().is_none());
    driver.advance(std::time::Duration::from_millis(3));
    driver.run_until_stalled();
    assert!(result.borrow().as_ref().unwrap().is_err());
    assert_eq!(
        driver
            .run_until(host.root(|context| async move { Ok(context.code_version()) }.boxed_local()))
            .unwrap(),
        1
    );
    assert_eq!(state.snapshot().unwrap(), before);
    assert_eq!(state.version(), 1);
    assert_eq!(new_starts.get(), 0);
    assert_eq!(old_starts.get(), 1);
    tx.send(()).unwrap();
    driver.run_until_stalled();
    assert_eq!(old_starts.get(), 2);
    assert_eq!(new_starts.get(), 0);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn failed_gated_migration_opens_gate_before_restarting_settled_old_task() {
    fn facet(
        contract: Rc<Contract>,
        version: u64,
        starts: Rc<Flag<usize>>,
        saved: Rc<RefCell<Option<Cell>>>,
    ) -> ExecutableFacet {
        ExecutableFacet::foreign("task-migration-failure", vec![], move |_, declarations| {
            let (starts, saved) = (starts.clone(), saved.clone());
            declarations.serve(
                contract.clone(),
                Rc::new(move |_, _, declaration| {
                    let state = declaration.state(
                        "value",
                        StateDeclaration::new(json!({"type":"integer"}), json!(7))
                            .version(version)
                            .with_migrate(|_, _| {
                                Err(RuntimeError::invalid(
                                    "migration_refused",
                                    "candidate migration failed",
                                ))
                            }),
                    )?;
                    *saved.borrow_mut() = Some(state);
                    let starts = starts.clone();
                    declaration.task(move |scope| {
                        starts.set(starts.get() + 1);
                        async move {
                            scope.signal().cancelled().await;
                            Ok(())
                        }
                        .boxed_local()
                    })?;
                    Ok(Implementation::new())
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..ServeOptions::default()
                },
            )
        })
        .unwrap()
    }
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("task-migration", json!(null)),
        driver.clone(),
    );
    let contract = token("migration-owner");
    let (old, new, saved) = (
        Rc::new(Flag::new(0)),
        Rc::new(Flag::new(0)),
        Rc::new(RefCell::new(None)),
    );
    driver
        .run_until(
            host.install_executable(
                facet(contract.clone(), 1, old.clone(), saved.clone()),
                LoadOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(old.get(), 1);
    let state = saved.borrow().clone().unwrap();
    let before = state.snapshot().unwrap();
    let cause = driver
        .run_until(host.reload(
            vec![facet(contract, 2, new.clone(), saved)],
            ReloadOptions::default(),
        ))
        .unwrap_err();
    assert_eq!(cause.reason, "migration_refused");
    driver.run_until_stalled();
    assert_eq!(state.snapshot().unwrap(), before);
    assert_eq!(state.version(), 1);
    assert_eq!(new.get(), 0);
    assert_eq!(
        old.get(),
        2,
        "the settled old task restarts only after restoration and opening its gate"
    );
    driver.run_until(host.close()).unwrap();
}

#[test]
fn compatible_task_stop_deadline_reports_overlap_and_close_waits_for_old_physical_settlement() {
    struct Fixture {
        starts: RefCell<Vec<u64>>,
        late_values: RefCell<Vec<u32>>,
        scopes: RefCell<Vec<TaskScope>>,
        gate: RefCell<Option<oneshot::Receiver<()>>>,
        closes: Flag<u32>,
    }
    fn facet(contract: Rc<Contract>, generation: u64, fixture: Rc<Fixture>) -> ExecutableFacet {
        ExecutableFacet::foreign("compatible-task-deadline", vec![], move |_, setup| {
            let fixture = fixture.clone();
            setup.serve(
                contract.clone(),
                Rc::new(move |_, _, declaration| {
                    let closed = fixture.clone();
                    let resource = declaration.resource(
                        "resource",
                        ResourceDeclaration::new(|_| async { Ok(7u32) }.boxed_local()).with_close(
                            move |_| {
                                closed.closes.set(closed.closes.get() + 1);
                                async { Ok(()) }.boxed_local()
                            },
                        ),
                    )?;
                    let fixture = fixture.clone();
                    declaration.task(move |scope| {
                        fixture.starts.borrow_mut().push(generation);
                        fixture.scopes.borrow_mut().push(scope.clone());
                        let fixture = fixture.clone();
                        let resource = resource.clone();
                        async move {
                            if generation == 1 {
                                let gate = fixture.gate.borrow_mut().take().expect("one old task");
                                // Intentionally ignores cooperative cancellation.
                                gate.await.unwrap();
                                fixture.late_values.borrow_mut().push(*resource.value()?);
                            } else {
                                scope.signal().cancelled().await;
                            }
                            Ok(())
                        }
                        .boxed_local()
                    })?;
                    Ok(Implementation::new())
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
        })
        .unwrap()
    }
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("compatible-task-deadline", json!(null)),
        driver.clone(),
    );
    let (release, gate) = oneshot::channel();
    let fixture = Rc::new(Fixture {
        starts: RefCell::new(vec![]),
        late_values: RefCell::new(vec![]),
        scopes: RefCell::new(vec![]),
        gate: RefCell::new(Some(gate)),
        closes: Flag::new(0),
    });
    let contract = token("compatible-task-owner");
    driver
        .run_until(
            host.install_executable(
                facet(contract.clone(), 1, fixture.clone()),
                LoadOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(&*fixture.starts.borrow(), &[1]);
    let old_scope = fixture.scopes.borrow()[0].clone();
    let report = Rc::new(RefCell::new(None));
    let observed = report.clone();
    let owner = host.clone();
    let candidate = facet(contract, 2, fixture.clone());
    driver.spawn(
        async move {
            *observed.borrow_mut() = Some(
                owner
                    .reload(
                        vec![candidate],
                        ReloadOptions {
                            task_stop_deadline: std::time::Duration::from_millis(2),
                            ..Default::default()
                        },
                    )
                    .await,
            );
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(report.borrow().is_none());
    assert!(
        old_scope.signal().is_cancelled(),
        "stop admission precedes waiting"
    );
    assert_eq!(&*fixture.starts.borrow(), &[1]);
    driver.advance(std::time::Duration::from_millis(2));
    driver.run_until_stalled();
    assert_eq!(
        report
            .borrow()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .current_version(),
        2
    );
    assert_eq!(
        &*fixture.starts.borrow(),
        &[1, 2],
        "compatible replacement starts after deadline"
    );
    assert!(
        host.errors()
            .iter()
            .any(|error| error.reason == "task_stop_deadline")
    );
    let late = driver
        .run_until(old_scope.root(|_| async { Ok(()) }.boxed_local()))
        .unwrap_err();
    assert_eq!(late.reason, "activity_stopped");
    assert_eq!(fixture.closes.get(), 0);
    let closing = Rc::new(RefCell::new(None));
    let observed = closing.clone();
    driver.spawn(
        async move {
            *observed.borrow_mut() = Some(host.close().await);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(
        closing.borrow().is_none(),
        "logical stop does not settle the old task"
    );
    assert_eq!(fixture.closes.get(), 0);
    release.send(()).unwrap();
    driver.run_until_stalled();
    closing.borrow().as_ref().unwrap().as_ref().unwrap();
    assert_eq!(&*fixture.late_values.borrow(), &[7]);
    assert_eq!(
        &*fixture.starts.borrow(),
        &[1, 2],
        "old stopped run never restarts after commit"
    );
    assert_eq!(fixture.closes.get(), 1);
}
