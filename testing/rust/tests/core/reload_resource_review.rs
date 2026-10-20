use futures::{FutureExt, StreamExt, channel::oneshot};
use serde_json::json;
use std::{
    cell::{Cell as Count, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{contracts::Contract, rpc::*, *};

type Callback = Rc<dyn Fn(u32)>;
type Sinks = Rc<RefCell<BTreeMap<&'static str, Callback>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    CandidateClose,
    Restoration,
}
type EventLog = Rc<RefCell<Vec<(&'static str, u32, u64)>>>;

struct Fixture {
    failure: Failure,
    refuse_candidate: bool,
    contracts: [Rc<Contract>; 2],
    sources: [EventSource<u32>; 2],
    sinks: Sinks,
    starts: Rc<RefCell<Vec<&'static str>>>,
    events: EventLog,
    resources: RefCell<BTreeMap<&'static str, Resource<u32>>>,
    cells: RefCell<BTreeMap<&'static str, Cell>>,
    a_opens: Rc<Count<usize>>,
    closes: Rc<RefCell<Vec<(&'static str, u64)>>>,
    close_started: Rc<Count<bool>>,
    close_gate: Rc<RefCell<Option<oneshot::Receiver<()>>>>,
}
impl Fixture {
    fn new(failure: Failure, refuse_candidate: bool) -> Rc<Self> {
        let sinks: Sinks = Rc::default();
        let sources = ["a", "b"].map(|label| {
            let callbacks = sinks.clone();
            EventSource::new(move |callback| {
                callbacks.borrow_mut().insert(label, callback);
                Ok(Box::new(|| {}))
            })
        });
        Rc::new(Self {
            failure,
            refuse_candidate,
            contracts: ["a", "b"].map(|label| {
                Rc::new(
                    Contract::local(
                        format!("restore-review-{label}"),
                        1,
                        json!({}),
                        BTreeMap::new(),
                    )
                    .unwrap(),
                )
            }),
            sources,
            sinks,
            starts: Rc::default(),
            events: Rc::default(),
            resources: RefCell::default(),
            cells: RefCell::default(),
            a_opens: Rc::new(Count::new(0)),
            closes: Rc::default(),
            close_started: Rc::new(Count::new(false)),
            close_gate: Rc::default(),
        })
    }
    fn facet(self: &Rc<Self>, version: u64) -> ExecutableFacet {
        let fixture = self.clone();
        ExecutableFacet::foreign("resource-review-facet", vec![], move |_, declaration| {
            for (index, label) in ["a", "b"].into_iter().enumerate() {
                let fixture = fixture.clone();
                declaration.serve(
                    fixture.contracts[index].clone(),
                    Rc::new(move |_, _, declaration| {
                        let refuse = fixture.refuse_candidate;
                        let cell = declaration.state(
                            "value",
                            StateDeclaration::new(json!({"type":"integer"}), json!(7))
                                .version(version)
                                .with_migrate(move |value, _| {
                                    if label == "b" && version > 1 && refuse {
                                        Err(RuntimeError::invalid(
                                            "candidate_refused",
                                            "candidate refused",
                                        ))
                                    } else {
                                        Ok(value)
                                    }
                                }),
                        )?;
                        let (opens, failure) = (fixture.a_opens.clone(), fixture.failure);
                        let (closes, started, gate) = (
                            fixture.closes.clone(),
                            fixture.close_started.clone(),
                            fixture.close_gate.clone(),
                        );
                        let mut resource = ResourceDeclaration::new(move |_| {
                            let refuse = if label == "a" && version == 1 {
                                opens.set(opens.get() + 1);
                                failure == Failure::Restoration && opens.get() > 1
                            } else {
                                false
                            };
                            async move {
                                if refuse {
                                    Err(RuntimeError::invalid("restore_open", "old opener failed"))
                                } else {
                                    Ok(41_u32)
                                }
                            }
                            .boxed_local()
                        })
                        .with_close(move |_| {
                            closes.borrow_mut().push((label, version));
                            let pending = if label == "a" && version > 1 {
                                started.set(true);
                                gate.borrow_mut().take()
                            } else {
                                None
                            };
                            async move {
                                if let Some(pending) = pending {
                                    pending.await.unwrap();
                                }
                                if label == "a" && version > 1 && failure == Failure::CandidateClose
                                {
                                    Err(RuntimeError::invalid(
                                        "candidate_close",
                                        "candidate closer failed",
                                    ))
                                } else {
                                    Ok(())
                                }
                            }
                            .boxed_local()
                        });
                        resource.version = if label == "a" { version } else { 1 };
                        resource.exclusive =
                            label == "a" && fixture.failure == Failure::Restoration;
                        let resource = declaration.resource("resource", resource)?;
                        if version == 1 {
                            fixture
                                .resources
                                .borrow_mut()
                                .insert(label, resource.clone());
                            fixture.cells.borrow_mut().insert(label, cell);
                        }
                        let (seen, value) = (fixture.events.clone(), resource.clone());
                        declaration.on_event(
                            "raw",
                            fixture.sources[index].clone(),
                            move |event, context| {
                                let (seen, value) = (seen.clone(), value.clone());
                                async move {
                                    assert_eq!(*value.value()?, 41);
                                    seen.borrow_mut()
                                        .push((label, event, context.code_version()));
                                    Ok(())
                                }
                                .boxed_local()
                            },
                        )?;
                        let starts = fixture.starts.clone();
                        declaration.task(move |scope| {
                            let (starts, value) = (starts.clone(), resource.clone());
                            async move {
                                assert_eq!(*value.value()?, 41);
                                starts.borrow_mut().push(label);
                                scope.signal().cancelled().await;
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
                )?;
            }
            Ok(())
        })
        .unwrap()
    }
    fn task_starts(&self, label: &str) -> usize {
        self.starts
            .borrow()
            .iter()
            .filter(|actual| **actual == label)
            .count()
    }
    fn emit(&self, label: &str, event: u32) {
        let sink = self.sinks.borrow()[label].clone();
        sink(event);
    }
}

fn abort_case(failure: Failure) {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("resource-review", json!(null)),
        driver.clone(),
    );
    let fixture = Fixture::new(failure, true);
    driver
        .run_until(
            host.install_executable(fixture.facet(1), LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    driver.run_until_stalled();
    assert_eq!((fixture.task_starts("a"), fixture.task_starts("b")), (1, 1));
    let pending = if failure == Failure::CandidateClose {
        let (release, wait) = oneshot::channel();
        *fixture.close_gate.borrow_mut() = Some(wait);
        Some(release)
    } else {
        None
    };
    let result = Rc::new(RefCell::new(None));
    let (reload_host, output, facet) = (host.clone(), result.clone(), fixture.facet(2));
    driver.spawn(
        async move {
            *output.borrow_mut() = Some(
                reload_host
                    .reload(vec![facet], ReloadOptions::default())
                    .await,
            );
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    if let Some(release) = pending {
        assert!(fixture.close_started.get());
        assert!(
            result.borrow().is_none(),
            "abort must join the actual candidate closer"
        );
        assert_eq!((fixture.task_starts("a"), fixture.task_starts("b")), (1, 1));
        release.send(()).unwrap();
        driver.run_until_stalled();
    }
    let error = result
        .borrow_mut()
        .take()
        .expect("abort physically settled")
        .unwrap_err();
    assert_eq!(error.reason, "candidate_refused");
    assert_eq!(
        fixture.task_starts("b"),
        2,
        "healthy object B must resume even when another object's cleanup or restore failed"
    );
    fixture.emit("b", 12);
    driver.run_until_stalled();
    assert!(fixture.events.borrow().contains(&("b", 12, 1)));
    let expected_a = if failure == Failure::Restoration {
        1
    } else {
        2
    };
    assert_eq!(fixture.task_starts("a"), expected_a);
    let old_a = fixture.resources.borrow()["a"].clone();
    if failure == Failure::Restoration {
        assert_eq!(old_a.value().unwrap_err().reason, "resource_restore");
        fixture.emit("a", 13);
        driver.run_until_stalled();
        assert!(
            !fixture
                .events
                .borrow()
                .iter()
                .any(|(label, _, _)| *label == "a")
        );
    } else {
        assert_eq!(*old_a.value().unwrap(), 41);
        fixture.emit("a", 13);
        driver.run_until_stalled();
        assert!(fixture.events.borrow().contains(&("a", 13, 1)));
    }
    for cell in fixture.cells.borrow().values() {
        assert_eq!(cell.version(), 1);
        assert_eq!(cell.snapshot().unwrap().value(), &json!(7));
    }
    let errors = host.errors();
    assert!(
        errors
            .iter()
            .any(|error| error.reason == "resource_cleanup")
    );
    let serialized = serde_json::to_string(&errors).unwrap();
    assert!(serialized.contains(if failure == Failure::Restoration {
        "restore_open"
    } else {
        "candidate_close"
    }));
    driver.run_until(host.close()).unwrap();
    assert_eq!(
        fixture
            .closes
            .borrow()
            .iter()
            .filter(|(label, version)| *label == "b" && *version == 1)
            .count(),
        1
    );
}

#[test]
fn candidate_closer_failure_does_not_prevent_either_healthy_installed_object_from_resuming() {
    abort_case(Failure::CandidateClose);
}

#[test]
fn one_failed_restoration_does_not_prevent_another_installed_objects_activities_from_resuming() {
    abort_case(Failure::Restoration);
}

#[test]
fn parent_abort_after_ready_joins_candidate_closer_then_resumes_healthy_installed_activities() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("child", json!(null)), driver.clone());
    host.set_rpc_identity(RootOrigin::new("child", "11111111111111111111111111111111").unwrap())
        .unwrap();
    let fixture = Fixture::new(Failure::CandidateClose, false);
    driver
        .run_until(
            host.install_executable(fixture.facet(1), LoadOptions::default())
                .unwrap(),
        )
        .unwrap();
    driver.run_until_stalled();
    let (release, wait) = oneshot::channel();
    *fixture.close_gate.borrow_mut() = Some(wait);
    let candidate = fixture.facet(2);
    host.set_reload_loader(Rc::new(move |_| {
        let candidate = candidate.clone();
        async move { Ok(candidate) }.boxed_local()
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
    let send = |frame| {
        raw.tx
            .try_send(Rc::from(serde_json::to_vec(&frame).unwrap()))
            .unwrap()
    };
    send(
        json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,
        "descriptor_version":1,"delta_codec":1,"peer":{"role":"host","identity":{
            "host":"parent","incarnation":"22222222222222222222222222222222"},"code_version":1},
        "receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,
            "state_window_bytes":262144}}}),
    );
    driver.run_until(connection.ready()).unwrap();
    send(
        json!({"t":"reload_prepare","transaction":1,"code_version":2,
        "facets":[{"name":"resource-review-facet","artifact":"v2"}],"remove":[],
        "timing":{"gate":100,"retire":100,"task_stop":1},"catchup":false}),
    );
    driver.run_until_stalled();
    let mut ready = false;
    while let Some(Some(event)) = raw.events.next().now_or_never() {
        if let LinkEvent::Message(bytes) = event {
            let frame: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            ready |= frame["t"] == "reload_vote" && frame["outcome"]["status"] == "ready";
        }
    }
    assert!(
        ready,
        "the candidate reached its actual irrevocable Ready boundary"
    );
    send(json!({"t":"reload_abort","transaction":1,"error":{
        "code":"aborted","reason":"parent_abort","message":"parent aborted","details":null}}));
    driver.run_until_stalled();
    assert!(fixture.close_started.get());
    assert_eq!((fixture.task_starts("a"), fixture.task_starts("b")), (1, 1));
    release.send(()).unwrap();
    driver.run_until_stalled();
    assert_eq!((fixture.task_starts("a"), fixture.task_starts("b")), (2, 2));
    fixture.emit("a", 21);
    fixture.emit("b", 22);
    driver.run_until_stalled();
    assert!(fixture.events.borrow().contains(&("a", 21, 1)));
    assert!(fixture.events.borrow().contains(&("b", 22, 1)));
    for resource in fixture.resources.borrow().values() {
        assert_eq!(*resource.value().unwrap(), 41);
    }
    assert!(
        host.errors()
            .iter()
            .any(|error| error.reason == "resource_cleanup")
    );
    assert!(
        !host
            .errors()
            .iter()
            .any(|error| error.reason == "host_failed")
    );
    driver.run_until(host.close()).unwrap();
}
