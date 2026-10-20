use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use std::{
    cell::{Cell as Flag, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{contracts::Contract, *};

type Sink = Rc<RefCell<Option<Rc<dyn Fn(u32)>>>>;

struct Fixture {
    contract: Rc<Contract>,
    source: EventSource<u32>,
    seen: Rc<RefCell<Vec<u32>>>,
    task_stopped: Rc<Flag<bool>>,
    task_gate: Rc<RefCell<Option<oneshot::Receiver<()>>>>,
    state: Rc<RefCell<Option<Cell>>>,
    opened: Rc<Flag<usize>>,
    closed: Rc<Flag<usize>>,
    refuse_migration: bool,
}

impl Fixture {
    fn facet(self: &Rc<Self>, version: u64) -> ExecutableFacet {
        let fixture = self.clone();
        ExecutableFacet::foreign("backlog-facet", vec![], move |_, declaration| {
            let fixture = fixture.clone();
            declaration.serve(
                fixture.contract.clone(),
                Rc::new(move |_, _, declaration| {
                    let state = declaration.state(
                        "value",
                        StateDeclaration::new(json!({"type":"integer"}), json!(7))
                            .version(version)
                            .with_migrate({
                                let refuse = fixture.refuse_migration;
                                move |old, _| {
                                    if refuse {
                                        Err(RuntimeError::invalid("migration_refused", "refused"))
                                    } else {
                                        Ok(old)
                                    }
                                }
                            }),
                    )?;
                    if version == 1 {
                        *fixture.state.borrow_mut() = Some(state);
                    }
                    let opened = fixture.opened.clone();
                    let closed = fixture.closed.clone();
                    declaration.resource(
                        "resource",
                        ResourceDeclaration::new(move |_| {
                            opened.set(opened.get() + 1);
                            async { Ok(41_u32) }.boxed_local()
                        })
                        .with_close(move |_| {
                            closed.set(closed.get() + 1);
                            async { Ok(()) }.boxed_local()
                        }),
                    )?;
                    let seen = fixture.seen.clone();
                    declaration.on_event_with_options(
                        "raw",
                        fixture.source.clone(),
                        move |event, _| {
                            seen.borrow_mut().push(event);
                            async { Ok(()) }.boxed_local()
                        },
                        ListenerOptions::new(2)?,
                    )?;
                    let (stopped, gate) = (fixture.task_stopped.clone(), fixture.task_gate.clone());
                    declaration.task(move |scope| {
                        let (stopped, gate) = (stopped.clone(), gate.borrow_mut().take());
                        async move {
                            scope.signal().cancelled().await;
                            stopped.set(true);
                            if let Some(gate) = gate {
                                gate.await.unwrap();
                            }
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
}

#[test]
fn paused_raw_backlog_aborts_candidate_preserves_data_and_reports_irreversible_stream_stop() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("backlog", json!(null)), driver.clone());
    let sink: Sink = Rc::new(RefCell::new(None));
    let detached = Rc::new(Flag::new(0));
    let (receiver, detaches) = (sink.clone(), detached.clone());
    let source = EventSource::new(move |callback| {
        *receiver.borrow_mut() = Some(callback);
        let detaches = detaches.clone();
        Ok(Box::new(move || detaches.set(detaches.get() + 1)))
    });
    let (release, wait) = oneshot::channel();
    let fixture = Rc::new(Fixture {
        contract: Rc::new(Contract::local("backlog-owner", 1, json!({}), BTreeMap::new()).unwrap()),
        source,
        seen: Rc::new(RefCell::new(Vec::new())),
        task_stopped: Rc::new(Flag::new(false)),
        task_gate: Rc::new(RefCell::new(Some(wait))),
        state: Rc::new(RefCell::new(None)),
        opened: Rc::new(Flag::new(0)),
        closed: Rc::new(Flag::new(0)),
        refuse_migration: false,
    });
    driver
        .run_until(
            host.install_executable(fixture.facet(1), LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    driver.run_until_stalled();
    let old = fixture.state.borrow().clone().unwrap();
    let before = old.snapshot().unwrap();
    let result = Rc::new(RefCell::new(None));
    let (reloading, received, candidate) = (host.clone(), result.clone(), fixture.facet(2));
    driver.spawn(
        async move {
            *received.borrow_mut() = Some(
                reloading
                    .reload(
                        vec![candidate],
                        ReloadOptions {
                            task_stop_deadline: std::time::Duration::from_millis(1),
                            ..ReloadOptions::default()
                        },
                    )
                    .await,
            );
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(fixture.task_stopped.get());
    driver.advance(std::time::Duration::from_millis(1));
    driver.run_until_stalled();
    assert!(result.borrow().is_none());
    let emit = sink.borrow().clone().unwrap();
    emit(1);
    emit(2);
    emit(3);
    driver.run_until_stalled();
    assert_eq!(detached.get(), 1);
    assert!(fixture.seen.borrow().is_empty());
    release.send(()).unwrap();
    driver.run_until_stalled();
    let cause = result
        .borrow_mut()
        .take()
        .expect("reload completed")
        .unwrap_err();
    assert_eq!(cause.reason, "listener_backlog");
    assert_eq!(old.snapshot().unwrap(), before);
    assert_eq!(old.version(), 1);
    assert_eq!((fixture.opened.get(), fixture.closed.get()), (1, 0));
    assert!(
        host.errors()
            .iter()
            .any(|error| error.reason == "listener_backlog")
    );
    emit(4);
    driver.run_until_stalled();
    assert!(
        fixture.seen.borrow().is_empty(),
        "overflowed raw stream stays stopped after abort"
    );
    driver.run_until(host.close()).unwrap();
    assert_eq!(detached.get(), 1);
    assert_eq!(fixture.closed.get(), 1);
}

#[test]
fn nonoverflowed_gate_events_replay_in_fifo_order_to_installed_handler_after_abort() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("backlog-replay", json!(null)),
        driver.clone(),
    );
    let sink: Sink = Rc::new(RefCell::new(None));
    let detached = Rc::new(Flag::new(0));
    let (receiver, detaches) = (sink.clone(), detached.clone());
    let source = EventSource::new(move |callback| {
        *receiver.borrow_mut() = Some(callback);
        let detaches = detaches.clone();
        Ok(Box::new(move || detaches.set(detaches.get() + 1)))
    });
    let (release, wait) = oneshot::channel();
    let fixture = Rc::new(Fixture {
        contract: Rc::new(Contract::local("replay-owner", 1, json!({}), BTreeMap::new()).unwrap()),
        source,
        seen: Rc::new(RefCell::new(Vec::new())),
        task_stopped: Rc::new(Flag::new(false)),
        task_gate: Rc::new(RefCell::new(Some(wait))),
        state: Rc::new(RefCell::new(None)),
        opened: Rc::new(Flag::new(0)),
        closed: Rc::new(Flag::new(0)),
        refuse_migration: true,
    });
    driver
        .run_until(
            host.install_executable(fixture.facet(1), LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    driver.run_until_stalled();
    let result = Rc::new(RefCell::new(None));
    let (reloading, received, candidate) = (host.clone(), result.clone(), fixture.facet(2));
    driver.spawn(
        async move {
            *received.borrow_mut() = Some(
                reloading
                    .reload(vec![candidate], ReloadOptions::default())
                    .await,
            );
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(fixture.task_stopped.get());
    assert!(result.borrow().is_none());
    let emit = sink.borrow().clone().unwrap();
    emit(1);
    emit(2);
    driver.run_until_stalled();
    assert!(fixture.seen.borrow().is_empty());
    release.send(()).unwrap();
    driver.run_until_stalled();
    let error = result
        .borrow_mut()
        .take()
        .expect("reload completed")
        .unwrap_err();
    assert_eq!(error.reason, "migration_refused");
    assert_eq!(&*fixture.seen.borrow(), &[1, 2]);
    assert_eq!(
        detached.get(),
        0,
        "installed source is reused across the aborted attempt"
    );
    emit(3);
    driver.run_until_stalled();
    assert_eq!(&*fixture.seen.borrow(), &[1, 2, 3]);
    assert_eq!(fixture.state.borrow().as_ref().unwrap().version(), 1);
    assert_eq!((fixture.opened.get(), fixture.closed.get()), (1, 0));
    assert!(host.errors().is_empty());
    driver.run_until(host.close()).unwrap();
    assert_eq!(detached.get(), 1);
    assert_eq!(fixture.closed.get(), 1);
}
