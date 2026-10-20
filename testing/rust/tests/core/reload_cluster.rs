//! Public coordinator acceptance over actual authenticated MemoryLink connections.
use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use std::{
    cell::{Cell as Flag, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};
use tembra_core::{
    contracts::{Contract, Member, Placement, ValueSchema},
    rpc::{Connection, ConnectionOptions, MemoryLinkOptions, RemoteFacet, RootOrigin, memory_link},
    *,
};

fn drive<T: 'static>(driver: &ManualDriver, future: impl Future<Output = Result<T>>) -> T {
    let timeout = driver.sleep(Duration::from_secs(30));
    match driver.run_until(futures::future::select(future.boxed_local(), timeout)) {
        futures::future::Either::Left((result, _)) => {
            result.unwrap_or_else(|error| panic!("public cluster operation failed: {error}"))
        }
        futures::future::Either::Right(_) => {
            panic!("public cluster operation exceeded manual deadline")
        }
    }
}

fn token(host: &str) -> Rc<Contract> {
    let integer = ValueSchema::Portable(json!({"type":"integer"}));
    let result = ValueSchema::Portable(json!({
        "type":"object", "additionalProperties":false,
        "properties": {
            "marker":{"type":"integer"},
            "epoch":{"type":"integer"},
            "count":{"type":"integer"}
        },
        "required":["marker","epoch","count"]
    }));
    let members = BTreeMap::from([
        (
            "count".into(),
            Member::State {
                schema: json!({"type":"integer"}),
            },
        ),
        (
            "read".into(),
            Member::Method {
                args: None,
                result: Some(result),
            },
        ),
        (
            "set".into(),
            Member::Method {
                args: Some(integer),
                result: None,
            },
        ),
    ]);
    Rc::new(
        Contract::remote(
            "cluster.counter",
            1,
            json!({}),
            Placement::Named { name: host.into() },
            BTreeMap::from([("counter".into(), members)]),
        )
        .unwrap(),
    )
}

fn facet(contract: Rc<Contract>, marker: u64, reject_migration: bool) -> ExecutableFacet {
    ExecutableFacet::foreign("counter", vec![], move |_, declarations| {
        declarations.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let mut spec = StateDeclaration::new(json!({"type":"integer"}), json!(5));
                if reject_migration {
                    spec = spec.version(2).with_migrate(|_, _| {
                        Err(RuntimeError::invalid(
                            "cluster_migration",
                            "participant rejected migration",
                        ))
                    });
                }
                let count = declaration.state("count", spec)?;
                let read = count.clone();
                Ok(Implementation::new()
                    .variant("counter")
                    .state("count", count.readable())
                    .json_method("read", move |_, context| {
                        let value = read.snapshot().map(|snapshot| {
                            Some(json!({
                                "marker":marker,
                                "epoch":context.code_version(),
                                "count":snapshot.value()
                            }))
                        });
                        async move { value }.boxed_local()
                    })
                    .json_method("set", move |value, context| {
                        let result = count.replace(
                            &context,
                            value.expect("validated present argument"),
                            ReplaceOptions::default(),
                        );
                        async move { result.map(|_| None) }.boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
    })
    .unwrap()
}

struct Node {
    host: Host,
    contract: Rc<Contract>,
    reference: Reference,
}
impl Node {
    fn new(driver: &Rc<ManualDriver>, name: &str, incarnation: &str) -> Self {
        let host = Host::new(
            HostOptions::new(name, json!("cluster-principal")),
            driver.clone(),
        );
        host.set_rpc_identity(RootOrigin::new(name, incarnation).unwrap())
            .unwrap();
        let contract = token(name);
        let startup = host
            .install_executable(facet(contract.clone(), 11, false), LoadOptions::default())
            .unwrap();
        drive(driver, startup);
        let reference = drive(
            driver,
            host.get(contract.clone(), json!({}), GetOptions::default()),
        );
        Self {
            host,
            contract,
            reference,
        }
    }
    fn read(&self, driver: &ManualDriver) -> Value {
        read(driver, &self.reference)
    }
    fn loader(&self, calls: Rc<Flag<usize>>) {
        let contract = self.contract.clone();
        self.host
            .set_reload_loader(Rc::new(move |artifact| {
                calls.set(calls.get() + 1);
                let contract = contract.clone();
                async move {
                    match artifact.artifact() {
                        "counter:2" => Ok(facet(contract, 22, false)),
                        "counter:3" => Ok(facet(contract, 33, false)),
                        "counter:refuse" => Ok(facet(contract, 22, true)),
                        _ => Err(RuntimeError::invalid(
                            "fixture_artifact",
                            "unknown explicit fixture artifact",
                        )),
                    }
                }
                .boxed_local()
            }))
            .unwrap();
    }
}

fn read(driver: &ManualDriver, reference: &Reference) -> Value {
    let reference = reference.clone();
    drive(driver, async move {
        reference.call_json("read", None, None).await
    })
    .unwrap()
}
fn set(driver: &ManualDriver, reference: &Reference, value: i32) {
    let reference = reference.clone();
    assert_eq!(
        drive(driver, async move {
            reference.call_json("set", Some(json!(value)), None).await
        }),
        None
    );
}
fn link(driver: &Rc<ManualDriver>, parent: &Node, child: &Node) -> (Connection, Connection) {
    let (outgoing, incoming, _) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let downstream = parent
        .host
        .accept_child(
            outgoing,
            "child",
            json!("cluster-principal"),
            ConnectionOptions::default(),
        )
        .unwrap();
    let upstream = child
        .host
        .connect_parent(
            incoming,
            "parent",
            json!("cluster-principal"),
            ConnectionOptions::default(),
        )
        .unwrap();
    let (a, b) = (downstream.clone(), upstream.clone());
    drive(driver, async move {
        futures::try_join!(a.ready(), b.ready()).map(|_| ())
    });
    parent.host.set_route("child", downstream.clone()).unwrap();
    (downstream, upstream)
}
fn nodes(driver: &Rc<ManualDriver>) -> (Node, Node) {
    (
        Node::new(driver, "parent", "11111111111111111111111111111111"),
        Node::new(driver, "child", "22222222222222222222222222222222"),
    )
}
fn remote(artifact: &str) -> Vec<RemoteFacet> {
    vec![RemoteFacet::new("counter", artifact).unwrap()]
}
fn close(driver: &ManualDriver, parent: &Node, child: &Node) {
    parent.reference.release();
    child.reference.release();
    let (parent, child) = (parent.host.clone(), child.host.clone());
    drive(driver, async move {
        futures::try_join!(parent.close(), child.close()).map(|_| ())
    });
}

#[test]
fn two_real_hosts_commit_and_new_epoch_routes_to_child_after_its_installation() {
    let driver = ManualDriver::new();
    let (parent, child) = nodes(&driver);
    let loads = Rc::new(Flag::new(0));
    child.loader(loads.clone());
    let _connections = link(&driver, &parent, &child);
    let routed = drive(
        &driver,
        parent
            .host
            .get(child.contract.clone(), json!({}), GetOptions::default()),
    );
    set(&driver, &parent.reference, 7);
    set(&driver, &routed, 9);
    let report = drive(
        &driver,
        parent.host.reload_cluster(
            vec![facet(parent.contract.clone(), 22, false)],
            remote("counter:2"),
            ReloadOptions::default(),
        ),
    );
    assert_eq!(report.local().current_version(), 2);
    assert_eq!(report.children().len(), 1);
    assert_eq!(report.children()[0].name(), "child");
    assert_eq!(report.children()[0].code_version(), 2);
    assert_eq!(report.children()[0].prepared_objects(), 1);
    assert_eq!(loads.get(), 1);
    assert_eq!(
        parent.read(&driver),
        json!({"marker":22,"epoch":2,"count":7})
    );
    assert_eq!(
        child.read(&driver),
        json!({"marker":22,"epoch":2,"count":9})
    );
    assert_eq!(
        read(&driver, &routed),
        json!({"marker":22,"epoch":2,"count":9})
    );
    driver.run_until_stalled();
    assert_eq!(
        routed
            .readable("count")
            .unwrap()
            .snapshot()
            .unwrap()
            .value(),
        &json!(9)
    );
    routed.release();
    close(&driver, &parent, &child);
}

#[test]
fn negative_participant_vote_preserves_installed_code_epoch_and_state_everywhere() {
    let driver = ManualDriver::new();
    let (parent, child) = nodes(&driver);
    child.loader(Rc::new(Flag::new(0)));
    let _connections = link(&driver, &parent, &child);
    set(&driver, &parent.reference, 7);
    set(&driver, &child.reference, 9);
    let error = drive(
        &driver,
        async {
            Ok(parent
                .host
                .reload_cluster(
                    vec![facet(parent.contract.clone(), 22, false)],
                    remote("counter:refuse"),
                    ReloadOptions::default(),
                )
                .await
                .unwrap_err())
        }
        .boxed_local(),
    );
    assert_eq!(error.code, ErrorCode::Invalid);
    assert_eq!(error.reason, "cluster_migration");
    assert_eq!(
        parent.read(&driver),
        json!({"marker":11,"epoch":1,"count":7})
    );
    assert_eq!(
        child.read(&driver),
        json!({"marker":11,"epoch":1,"count":9})
    );
    close(&driver, &parent, &child);
}

#[test]
fn missing_child_loader_is_a_negative_vote_not_a_partial_parent_commit() {
    let driver = ManualDriver::new();
    let (parent, child) = nodes(&driver);
    let _connections = link(&driver, &parent, &child);
    let future = parent.host.reload_cluster(
        vec![facet(parent.contract.clone(), 22, false)],
        remote("counter:2"),
        ReloadOptions::default(),
    );
    let error = drive(&driver, async move { Ok(future.await.unwrap_err()) });
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_eq!(error.reason, "reload_loader");
    assert_eq!(
        parent.read(&driver),
        json!({"marker":11,"epoch":1,"count":5})
    );
    assert_eq!(
        child.read(&driver),
        json!({"marker":11,"epoch":1,"count":5})
    );
    close(&driver, &parent, &child);
}

#[test]
fn abort_during_actual_child_loading_keeps_old_code_and_close_waits_for_loader_settlement() {
    let driver = ManualDriver::new();
    let (parent, child) = nodes(&driver);
    let (release, wait) = oneshot::channel();
    let wait = Rc::new(RefCell::new(Some(wait)));
    let entered = Rc::new(Flag::new(false));
    let settled = Rc::new(Flag::new(false));
    let (waiting, started, finished, contract) = (
        wait.clone(),
        entered.clone(),
        settled.clone(),
        child.contract.clone(),
    );
    child
        .host
        .set_reload_loader(Rc::new(move |_| {
            started.set(true);
            let (wait, settled, contract) = (
                waiting.borrow_mut().take().unwrap(),
                finished.clone(),
                contract.clone(),
            );
            async move {
                wait.await.unwrap();
                settled.set(true);
                Ok(facet(contract, 22, false))
            }
            .boxed_local()
        }))
        .unwrap();
    let _connections = link(&driver, &parent, &child);
    let cancellation = CancellationToken::new();
    let transaction = parent.host.reload_cluster(
        vec![facet(parent.contract.clone(), 22, false)],
        remote("counter:2"),
        ReloadOptions {
            cancellation: Some(cancellation.clone()),
            ..ReloadOptions::default()
        },
    );
    let result = Rc::new(RefCell::new(None));
    let received = result.clone();
    driver.spawn(
        async move {
            *received.borrow_mut() = Some(transaction.await);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(entered.get());
    assert!(!settled.get());
    assert!(result.borrow().is_none());
    cancellation.cancel(RuntimeError::aborted(
        "fixture_cancel",
        "abort during participant loading",
    ));
    driver.run_until_stalled();
    let error = result
        .borrow_mut()
        .take()
        .expect("parent observes predecision cancellation")
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Aborted);
    assert_eq!(error.reason, "fixture_cancel");
    assert_eq!(
        parent.read(&driver),
        json!({"marker":11,"epoch":1,"count":5})
    );
    assert_eq!(
        child.read(&driver),
        json!({"marker":11,"epoch":1,"count":5})
    );
    let closed = Rc::new(RefCell::new(None));
    let (closing, observed) = (child.host.clone(), closed.clone());
    driver.spawn(
        async move {
            *observed.borrow_mut() = Some(closing.close().await);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(
        closed.borrow().is_none(),
        "logical abort does not prove loader settlement"
    );
    assert!(!settled.get());
    release.send(()).unwrap();
    driver.run_until_stalled();
    assert!(settled.get());
    assert!(
        closed
            .borrow_mut()
            .take()
            .expect("child closes after actual loader")
            .is_ok()
    );
    parent.reference.release();
    child.reference.release();
    drive(&driver, parent.host.close());
}

#[test]
fn behind_child_loads_remembered_full_deployment_before_parent_route_becomes_ready() {
    let driver = ManualDriver::new();
    let (parent, child) = nodes(&driver);
    for (version, marker) in [("counter:2", 22), ("counter:3", 33)] {
        let report = drive(
            &driver,
            parent.host.reload_cluster(
                vec![facet(parent.contract.clone(), marker, false)],
                remote(version),
                ReloadOptions::default(),
            ),
        );
        assert!(report.children().is_empty());
    }
    assert_eq!(parent.read(&driver)["epoch"], json!(3));
    assert_eq!(child.read(&driver)["epoch"], json!(1));
    let loads = Rc::new(Flag::new(0));
    child.loader(loads.clone());
    let _connections = link(&driver, &parent, &child);
    assert_eq!(
        loads.get(),
        1,
        "catch-up resolves the remembered current full deployment once"
    );
    assert_eq!(
        child.read(&driver),
        json!({"marker":33,"epoch":3,"count":5})
    );
    let routed = drive(
        &driver,
        parent
            .host
            .get(child.contract.clone(), json!({}), GetOptions::default()),
    );
    assert_eq!(
        read(&driver, &routed),
        json!({"marker":33,"epoch":3,"count":5})
    );
    routed.release();
    close(&driver, &parent, &child);
}

#[test]
fn child_cannot_coordinate_and_trusted_hierarchy_cannot_silently_reload_one_host() {
    let driver = ManualDriver::new();
    let (parent, child) = nodes(&driver);
    let _connections = link(&driver, &parent, &child);
    let transaction = child
        .host
        .reload_cluster(Vec::new(), Vec::new(), ReloadOptions::default());
    let error = drive(&driver, async move { Ok(transaction.await.unwrap_err()) });
    assert_eq!(error.code, ErrorCode::Forbidden);
    assert_eq!(error.reason, "reload_parent");
    for node in [&parent, &child] {
        let local = node.host.reload(Vec::new(), ReloadOptions::default());
        let error = drive(&driver, async move { Ok(local.await.unwrap_err()) });
        assert_eq!(error.code, ErrorCode::Forbidden);
        assert_eq!(error.reason, "reload_coordination");
        assert_eq!(node.read(&driver), json!({"marker":11,"epoch":1,"count":5}));
    }
    close(&driver, &parent, &child);
}

fn announcement(registry: Registry, marker: u64) -> ExecutableFacet {
    let contract =
        Rc::new(Contract::local("cluster.announcement", 1, json!({}), BTreeMap::new()).unwrap());
    ExecutableFacet::foreign("announcement", vec![], move |_, declarations| {
        let registry = registry.clone();
        declarations.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                declaration.register(&registry, "epoch", json!(marker))?;
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

#[test]
fn commit_observer_reentrant_new_root_call_reaches_child_after_its_commit_frame() {
    let driver = ManualDriver::new();
    let (parent, child) = nodes(&driver);
    child.loader(Rc::new(Flag::new(0)));
    let registry = Registry::new("cluster.commit", ConflictPolicy::Error).unwrap();
    drive(
        &driver,
        parent
            .host
            .install_executable(announcement(registry.clone(), 11), LoadOptions::default())
            .unwrap(),
    );
    let _connections = link(&driver, &parent, &child);
    let routed = drive(
        &driver,
        parent
            .host
            .get(child.contract.clone(), json!({}), GetOptions::default()),
    );
    let observer_returned = Rc::new(Flag::new(false));
    let started = Rc::new(Flag::new(false));
    let result = Rc::new(RefCell::new(None));
    let (caller, reference, executor, returned, admitted, delivered) = (
        parent.host.clone(),
        routed.clone(),
        driver.clone(),
        observer_returned.clone(),
        started.clone(),
        result.clone(),
    );
    let subscription = parent
        .host
        .registry(&registry)
        .unwrap()
        .subscribe(move |snapshot| {
            if snapshot
                .get("epoch")
                .is_none_or(|entry| entry.data() != &json!(22))
            {
                return Ok(());
            }
            assert!(
                !returned.get(),
                "commit notification precedes the reload result observer"
            );
            let (caller, reference, admitted, returned, delivered) = (
                caller.clone(),
                reference.clone(),
                admitted.clone(),
                returned.clone(),
                delivered.clone(),
            );
            let mut work = async move {
                let outcome = caller
                    .root(move |context| {
                        assert!(!returned.get());
                        assert_eq!(context.code_version(), 2);
                        admitted.set(true);
                        async move { reference.call_json("read", None, Some(&context)).await }
                            .boxed_local()
                    })
                    .await;
                *delivered.borrow_mut() = Some(outcome);
            }
            .boxed_local();
            // Poll the actual managed root and routed call at the notification boundary.
            // MemoryLink only queues bytes here; its peer dispatch remains Driver-owned.
            let waker = futures::task::noop_waker();
            let mut context = std::task::Context::from_waker(&waker);
            if work.as_mut().poll(&mut context).is_pending() {
                executor.spawn(work);
            }
            Ok(())
        })
        .unwrap();
    let transaction = parent.host.reload_cluster(
        vec![
            facet(parent.contract.clone(), 22, false),
            announcement(registry, 22),
        ],
        remote("counter:2"),
        ReloadOptions::default(),
    );
    let report = drive(&driver, async {
        let report = transaction.await?;
        observer_returned.set(true);
        Ok(report)
    });
    assert_eq!(report.local().current_version(), 2);
    assert!(
        started.get(),
        "actual new-root call was polled during commit notification"
    );
    driver.run_until_stalled();
    let reply = result
        .borrow_mut()
        .take()
        .expect("reentrant routed call settled")
        .unwrap();
    assert_eq!(reply, Some(json!({"marker":22,"epoch":2,"count":5})));
    drop(subscription);
    routed.release();
    close(&driver, &parent, &child);
}
