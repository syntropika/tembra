use futures::{FutureExt, StreamExt, channel::oneshot};
use serde_json::{Value, json};
use std::{
    cell::{Cell as Count, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{contracts::Contract, rpc::*, *};

type Sink = Rc<RefCell<Option<Rc<dyn Fn(u32)>>>>;

struct Fixture {
    contract: Rc<Contract>,
    source: EventSource<u32>,
    sink: Sink,
    detach: Rc<Count<usize>>,
    seen: Rc<RefCell<Vec<(u32, u64)>>>,
    wait: Rc<RefCell<Option<oneshot::Receiver<()>>>>,
    initial_listener: bool,
    state: RefCell<Option<Cell>>,
}
impl Fixture {
    fn new(initial_listener: bool) -> Rc<Self> {
        let sink: Sink = Rc::default();
        let detach = Rc::new(Count::new(0));
        let (callbacks, detached) = (sink.clone(), detach.clone());
        let source = EventSource::new(move |callback| {
            *callbacks.borrow_mut() = Some(callback);
            let detached = detached.clone();
            Ok(Box::new(move || detached.set(detached.get() + 1)))
        });
        Rc::new(Self {
            contract: Rc::new(
                Contract::local("review-listener", 1, json!({}), BTreeMap::new()).unwrap(),
            ),
            source,
            sink,
            detach,
            seen: Rc::default(),
            wait: Rc::default(),
            initial_listener,
            state: RefCell::default(),
        })
    }
    fn facet(self: &Rc<Self>, version: u64, gate: bool) -> ExecutableFacet {
        let fixture = self.clone();
        ExecutableFacet::foreign("review-listener-facet", vec![], move |_, declaration| {
            let fixture = fixture.clone();
            declaration.serve(
                fixture.contract.clone(),
                Rc::new(move |_, _, declaration| {
                    let state = declaration.state(
                        "value",
                        StateDeclaration::new(json!({"type":"integer"}), json!(7))
                            .version(if gate { version } else { 1 }),
                    )?;
                    *fixture.state.borrow_mut() = Some(state);
                    if fixture.initial_listener || version > 1 {
                        let (seen, wait) = (fixture.seen.clone(), fixture.wait.clone());
                        declaration.on_event_with_options(
                            "raw",
                            fixture.source.clone(),
                            move |event, context| {
                                let seen = seen.clone();
                                let wait = wait.borrow_mut().take();
                                async move {
                                    seen.borrow_mut().push((event, context.code_version()));
                                    if let Some(wait) = wait {
                                        wait.await.unwrap();
                                    }
                                    Ok(())
                                }
                                .boxed_local()
                            },
                            ListenerOptions::new(2)?,
                        )?;
                    }
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
    fn emit(&self, event: u32) {
        self.sink.borrow().clone().unwrap()(event);
    }
}

fn send(raw: &Link, frame: Value) {
    raw.tx
        .try_send(Rc::from(serde_json::to_vec(&frame).unwrap()))
        .unwrap();
}
fn drain(driver: &ManualDriver, raw: &mut Link) -> Vec<Value> {
    driver.run_until_stalled();
    let mut frames = Vec::new();
    while let Some(Some(event)) = raw.events.next().now_or_never() {
        if let LinkEvent::Message(bytes) = event {
            frames.push(serde_json::from_slice(&bytes).unwrap());
        }
    }
    frames
}
fn prepare(raw: &Link, transaction: u64, version: u64) {
    send(
        raw,
        json!({"t":"reload_prepare","transaction":transaction,"code_version":version,
        "facets":[{"name":"review-listener-facet","artifact":version.to_string()}],"remove":[],
        "timing":{"gate":100,"retire":100,"task_stop":1},"catchup":false}),
    );
}

fn ready_overflow_case(reused: bool) {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("child", json!(null)), driver.clone());
    host.set_rpc_identity(RootOrigin::new("child", "11111111111111111111111111111111").unwrap())
        .unwrap();
    let fixture = Fixture::new(reused);
    driver
        .run_until(
            host.install_executable(fixture.facet(1, true), LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    let loaded = fixture.clone();
    host.set_reload_loader(Rc::new(move |artifact| {
        let facet = loaded.facet(artifact.artifact().parse().unwrap(), true);
        async move { Ok(facet) }.boxed_local()
    }))
    .unwrap();
    let (mut raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
    let connection = host
        .connect_parent(
            incoming,
            "parent",
            json!("parent"),
            ConnectionOptions::default(),
        )
        .unwrap();
    send(
        &raw,
        json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,
            "descriptor_version":1,"delta_codec":1,
            "peer":{"role":"host","identity":{"host":"parent",
                "incarnation":"22222222222222222222222222222222"},"code_version":1},
            "receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,
                "state_window_bytes":262144}}}),
    );
    driver.run_until(connection.ready()).unwrap();
    drain(&driver, &mut raw);
    prepare(&raw, 1, 2);
    let frames = drain(&driver, &mut raw);
    assert!(
        frames
            .iter()
            .any(|frame| frame["t"] == "reload_vote" && frame["outcome"]["status"] == "ready")
    );
    fixture.emit(1);
    fixture.emit(2);
    fixture.emit(3);
    driver.run_until_stalled();
    assert_eq!(fixture.detach.get(), 1);
    send(
        &raw,
        json!({"t":"reload_commit","transaction":1,"code_version":2}),
    );
    let frames = drain(&driver, &mut raw);
    assert!(
        frames.iter().any(|frame| frame["t"] == "reload_ack"),
        "a Ready vote remains adoptable after ordinary event overflow: {frames:?}"
    );
    let epoch = driver
        .run_until(host.root(|context| async move { Ok(context.code_version()) }.boxed_local()))
        .unwrap();
    assert_eq!(epoch, 2);
    fixture.emit(4);
    driver.run_until_stalled();
    assert!(
        fixture.seen.borrow().is_empty(),
        "commit must not revive the stopped stream"
    );
    assert!(
        host.errors()
            .iter()
            .any(|error| error.reason == "listener_backlog")
    );
    assert!(
        !host
            .errors()
            .iter()
            .any(|error| error.reason == "host_failed")
    );
    send(&raw, json!({"t":"reload_retire","code_version":1}));
    prepare(&raw, 2, 3);
    let frames = drain(&driver, &mut raw);
    assert!(
        frames
            .iter()
            .any(|frame| frame["t"] == "reload_vote" && frame["outcome"]["status"] == "ready")
    );
    send(
        &raw,
        json!({"t":"reload_commit","transaction":2,"code_version":3}),
    );
    assert!(
        drain(&driver, &mut raw)
            .iter()
            .any(|frame| frame["t"] == "reload_ack")
    );
    fixture.emit(5);
    driver.run_until_stalled();
    assert_eq!(&*fixture.seen.borrow(), &[(5, 3)]);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn staged_listener_overflow_after_ready_still_commits_and_acknowledges() {
    ready_overflow_case(false);
}

#[test]
fn paused_reused_listener_overflow_after_ready_still_commits_and_acknowledges() {
    ready_overflow_case(true);
}

#[test]
fn historical_installed_overflow_does_not_poison_a_fresh_reload_subscription() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("local", json!(null)), driver.clone());
    let fixture = Fixture::new(true);
    driver
        .run_until(
            host.install_executable(fixture.facet(1, false), LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    let (release, wait) = oneshot::channel();
    *fixture.wait.borrow_mut() = Some(wait);
    fixture.emit(0);
    driver.run_until_stalled();
    fixture.emit(1);
    fixture.emit(2);
    fixture.emit(3);
    driver.run_until_stalled();
    assert_eq!(fixture.detach.get(), 1);
    release.send(()).unwrap();
    driver.run_until_stalled();
    let result =
        driver.run_until(host.reload(vec![fixture.facet(2, false)], ReloadOptions::default()));
    assert!(
        result.is_ok(),
        "a historical stopped-stream failure is not a new transaction failure: {result:?}"
    );
    fixture.emit(4);
    driver.run_until_stalled();
    assert_eq!(&*fixture.seen.borrow(), &[(0, 1), (4, 2)]);
    driver.run_until(host.close()).unwrap();
    assert_eq!(fixture.detach.get(), 2);
}

#[test]
fn parent_local_ready_seals_listener_before_child_vote_and_classifies_temporary_conflicts() {
    for gated in [false, true] {
        let driver = ManualDriver::new();
        let parent = Host::new(HostOptions::new("parent", Value::Null), driver.clone());
        let child = Host::new(HostOptions::new("child", Value::Null), driver.clone());
        parent
            .set_rpc_identity(
                RootOrigin::new("parent", "11111111111111111111111111111111").unwrap(),
            )
            .unwrap();
        child
            .set_rpc_identity(RootOrigin::new("child", "22222222222222222222222222222222").unwrap())
            .unwrap();
        let fixture = Fixture::new(false);
        driver
            .run_until(
                parent
                    .install_executable(fixture.facet(1, gated), LoadOptions::default())
                    .unwrap(),
            )
            .unwrap();
        let late = Rc::new(Contract::local("late-address", 1, json!({}), BTreeMap::new()).unwrap());
        let factories = Rc::new(Count::new(0));
        let entered = factories.clone();
        parent
            .serve(
                late.clone(),
                Rc::new(move |_, _, _| {
                    entered.set(entered.get() + 1);
                    Ok(Implementation::new())
                }),
                ServeOptions::default(),
            )
            .unwrap();
        let loading = Rc::new(Count::new(false));
        let entered = loading.clone();
        let (release, wait) = oneshot::channel();
        let wait = Rc::new(RefCell::new(Some(wait)));
        child
            .set_reload_loader(Rc::new(move |remote| {
                assert_eq!(remote.name(), "child-noop");
                entered.set(true);
                let wait = wait.borrow_mut().take().expect("one actual child loader");
                async move {
                    wait.await.unwrap();
                    ExecutableFacet::foreign("child-noop", vec![], |_, _| Ok(()))
                }
                .boxed_local()
            }))
            .unwrap();
        let (left, right, _) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
        let accepted = parent
            .accept_child(left, "child", Value::Null, ConnectionOptions::default())
            .unwrap();
        let upstream = child
            .connect_parent(right, "parent", Value::Null, ConnectionOptions::default())
            .unwrap();
        driver.run_until(accepted.ready()).unwrap();
        driver.run_until(upstream.ready()).unwrap();
        let result = Rc::new(RefCell::new(None));
        let completed = result.clone();
        let owner = parent.clone();
        let candidate = fixture.facet(2, gated);
        driver.spawn(
            async move {
                *completed.borrow_mut() = Some(
                    owner
                        .reload_cluster(
                            vec![candidate],
                            vec![RemoteFacet::new("child-noop", "version2").unwrap()],
                            ReloadOptions::default(),
                        )
                        .await,
                );
            }
            .boxed_local(),
        );
        driver.run_until_stalled();
        assert!(
            loading.get(),
            "actual child preparation begins after the parent local seal"
        );
        assert!(
            result.borrow().is_none(),
            "child cannot vote before its loader settles"
        );
        let hold_error = parent
            .hold(late, json!({}), GetOptions::default())
            .err()
            .unwrap();
        let install_error = parent
            .serve(
                Rc::new(Contract::local("late-service", 1, json!({}), BTreeMap::new()).unwrap()),
                Rc::new(|_, _, _| Ok(Implementation::new())),
                ServeOptions::default(),
            )
            .unwrap_err();
        let predicates = Count::new(0);
        let disposal_error = driver
            .run_until(parent.invalidate_principal(|_| {
                predicates.set(predicates.get() + 1);
                false
            }))
            .unwrap_err();
        let changed = Rc::new(Count::new(false));
        let touched = changed.clone();
        let state = fixture.state.borrow().as_ref().unwrap().clone();
        let write_error = driver
            .run_until(parent.root(move |context| {
                async move {
                    state.change(&context, move |_| {
                        touched.set(true);
                        Ok(())
                    })?;
                    Ok(())
                }
                .boxed_local()
            }))
            .unwrap_err();
        assert_eq!(predicates.get(), 0);
        assert_eq!(factories.get(), 0);
        assert!(!changed.get());
        assert_eq!(
            fixture
                .state
                .borrow()
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .value(),
            &json!(7)
        );
        fixture.emit(1);
        fixture.emit(2);
        fixture.emit(3);
        driver.run_until_stalled();
        assert_eq!(fixture.detach.get(), 1);
        assert!(fixture.seen.borrow().is_empty());
        assert!(
            parent
                .errors()
                .iter()
                .any(|error| error.reason == "listener_backlog")
        );
        assert!(result.borrow().is_none());
        release.send(()).unwrap();
        driver.run_until_stalled();
        assert_eq!(
            result
                .borrow()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .local()
                .current_version(),
            2
        );
        assert_eq!(
            driver
                .run_until(child.root(|ctx| async move { Ok(ctx.code_version()) }.boxed_local()))
                .unwrap(),
            2
        );
        fixture.emit(4);
        driver.run_until_stalled();
        assert!(
            fixture.seen.borrow().is_empty(),
            "commit must not revive the overflowed listener"
        );
        driver.run_until(parent.close()).unwrap();
        driver.run_until(child.close()).unwrap();
        for error in [hold_error, install_error, disposal_error, write_error] {
            assert_eq!(error.reason, "upgrade_conflict");
            assert_eq!(
                error.code,
                ErrorCode::UpgradeConflict,
                "all temporary reload admission conflicts share the checked category"
            );
            assert!(error.retryable());
        }
    }
}
