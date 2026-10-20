//! Independent public regression cases for one-level coordinator review.
use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};
use tembra_core::{
    contracts::{Contract, Member, Placement, ValueSchema},
    rpc::{
        CloseReason, Connection, ConnectionOptions, MemoryControl, MemoryLinkOptions, RemoteFacet,
        RootOrigin, memory_link,
    },
    *,
};

fn drive<T>(driver: &ManualDriver, future: impl Future<Output = Result<T>>) -> T {
    match driver.run_until(futures::future::select(
        future.boxed_local(),
        driver.sleep(Duration::from_secs(30)),
    )) {
        futures::future::Either::Left((result, _)) => result.unwrap(),
        futures::future::Either::Right(_) => panic!("coordinator review deadline elapsed"),
    }
}

struct Node {
    name: String,
    host: Host,
    contract: Rc<Contract>,
    reference: Reference,
}
fn facet(contract: Rc<Contract>, marker: u64) -> ExecutableFacet {
    let announcement = Rc::new(
        Contract::local("cluster.review.announcement", 1, json!({}), BTreeMap::new()).unwrap(),
    );
    let registry = Registry::new("cluster.review.commit", ConflictPolicy::Error).unwrap();
    ExecutableFacet::foreign("reviewed", vec![], move |_, declarations| {
        declarations.serve(
            contract.clone(),
            Rc::new(move |_, _, _| {
                Ok(Implementation::new()
                    .variant("view")
                    .json_method("read", move |_, context| {
                        let value = json!({"marker":marker,"epoch":context.code_version()});
                        async move { Ok(Some(value)) }.boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )?;
        let registry = registry.clone();
        declarations.serve(
            announcement.clone(),
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
impl Node {
    fn new(driver: &Rc<ManualDriver>, name: &str, incarnation: &str) -> Self {
        let host = Host::new(HostOptions::new(name, json!("cluster")), driver.clone());
        host.set_rpc_identity(RootOrigin::new(name, incarnation).unwrap())
            .unwrap();
        let output = ValueSchema::Portable(json!({
            "type":"object", "additionalProperties":false,
            "properties":{"marker":{"type":"integer"},"epoch":{"type":"integer"}},
            "required":["marker","epoch"]
        }));
        let members = BTreeMap::from([(
            "read".into(),
            Member::Method {
                args: None,
                result: Some(output),
            },
        )]);
        let contract = Rc::new(
            Contract::remote(
                "cluster.review",
                1,
                json!({}),
                Placement::Named { name: name.into() },
                BTreeMap::from([("view".into(), members)]),
            )
            .unwrap(),
        );
        drive(
            driver,
            host.install_executable(facet(contract.clone(), 11), LoadOptions::default())
                .unwrap(),
        );
        let reference = drive(
            driver,
            host.get(contract.clone(), json!({}), GetOptions::default()),
        );
        Self {
            name: name.into(),
            host,
            contract,
            reference,
        }
    }
}

fn connect(
    driver: &Rc<ManualDriver>,
    parent: &Node,
    child: &Node,
) -> (Connection, Connection, MemoryControl) {
    let (a, b, control) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    (
        parent
            .host
            .accept_child(
                a,
                child.name.clone(),
                json!("cluster"),
                ConnectionOptions::default(),
            )
            .unwrap(),
        child
            .host
            .connect_parent(
                b,
                parent.name.clone(),
                json!("cluster"),
                ConnectionOptions::default(),
            )
            .unwrap(),
        control,
    )
}

#[test]
fn link_loss_after_actual_child_commit_reports_unknown_outcome_without_rollback() {
    let driver = ManualDriver::new();
    let parent = Node::new(&driver, "parent", "44444444444444444444444444444444");
    let child = Node::new(&driver, "child", "55555555555555555555555555555555");
    let token = child.contract.clone();
    child
        .host
        .set_reload_loader(Rc::new(move |_| {
            let token = token.clone();
            async move { Ok(facet(token, 22)) }.boxed_local()
        }))
        .unwrap();
    let connection = connect(&driver, &parent, &child);
    drive(&driver, async {
        futures::try_join!(connection.0.ready(), connection.1.ready()).map(|_| ())
    });
    let registry = Registry::new("cluster.review.commit", ConflictPolicy::Error).unwrap();
    let observed = Rc::new(Cell::new(false));
    let (paused, control) = (observed.clone(), connection.2.clone());
    let subscription = child
        .host
        .registry(&registry)
        .unwrap()
        .subscribe(move |snapshot| {
            if snapshot
                .get("epoch")
                .is_some_and(|entry| entry.data() == &json!(22))
            {
                assert_eq!(snapshot.code_version(), 2);
                control.pause(1, true);
                paused.set(true);
            }
            Ok(())
        })
        .unwrap();
    let transaction = parent.host.reload_cluster(
        vec![facet(parent.contract.clone(), 22)],
        vec![RemoteFacet::new("reviewed", "ready:2").unwrap()],
        ReloadOptions::default(),
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
    assert!(
        observed.get(),
        "a real child commit notification paused acknowledgement transport"
    );
    assert!(connection.2.queued_bytes(1) > 0);
    assert!(
        result.borrow().is_none(),
        "parent still awaits commit acknowledgement"
    );
    assert_eq!(
        read(&driver, &parent.reference),
        json!({"marker":22,"epoch":2})
    );
    assert_eq!(
        read(&driver, &child.reference),
        json!({"marker":22,"epoch":2})
    );
    connection.0.close(CloseReason::closed());
    driver.run_until_stalled();
    let error = result
        .borrow_mut()
        .take()
        .expect("link loss settles the observer")
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_eq!(error.reason, "reload_unknown_outcome");
    assert_eq!(
        read(&driver, &parent.reference),
        json!({"marker":22,"epoch":2})
    );
    assert_eq!(
        read(&driver, &child.reference),
        json!({"marker":22,"epoch":2})
    );
    drop(subscription);
    parent.reference.release();
    child.reference.release();
    drive(&driver, async {
        futures::try_join!(parent.host.close(), child.host.close()).map(|_| ())
    });
}
#[test]
fn acknowledgement_failure_reports_every_participant_after_the_commit_decision() {
    let driver = ManualDriver::new();
    let parent = Node::new(&driver, "parent", "11111111111111111111111111111111");
    let lost = Node::new(&driver, "lost", "22222222222222222222222222222222");
    let healthy = Node::new(&driver, "healthy", "33333333333333333333333333333333");
    for node in [&lost, &healthy] {
        let contract = node.contract.clone();
        node.host
            .set_reload_loader(Rc::new(move |_| {
                let contract = contract.clone();
                async move { Ok(facet(contract, 22)) }.boxed_local()
            }))
            .unwrap();
    }
    let lost_link = connect(&driver, &parent, &lost);
    let healthy_link = connect(&driver, &parent, &healthy);
    drive(&driver, async {
        futures::try_join!(
            lost_link.0.ready(),
            lost_link.1.ready(),
            healthy_link.0.ready(),
            healthy_link.1.ready()
        )
        .map(|_| ())
    });
    let registry = Registry::new("cluster.review.commit", ConflictPolicy::Error).unwrap();
    let control = lost_link.2.clone();
    let subscription = lost
        .host
        .registry(&registry)
        .unwrap()
        .subscribe(move |snapshot| {
            if snapshot.code_version() == 2 {
                control.pause(1, true);
            }
            Ok(())
        })
        .unwrap();
    let transaction = parent.host.reload_cluster(
        vec![facet(parent.contract.clone(), 22)],
        vec![RemoteFacet::new("reviewed", "ready:2").unwrap()],
        ReloadOptions::default(),
    );
    let result = Rc::new(RefCell::new(None));
    let received = result.clone();
    driver.spawn(async move { *received.borrow_mut() = Some(transaction.await) }.boxed_local());
    driver.run_until_stalled();
    assert!(result.borrow().is_none());
    assert!(lost_link.2.queued_bytes(1) > 0);
    for node in [&parent, &lost, &healthy] {
        assert_eq!(
            read(&driver, &node.reference),
            json!({"marker":22,"epoch":2})
        );
    }
    lost_link.0.close(CloseReason::closed());
    driver.run_until_stalled();
    let error = result.borrow_mut().take().unwrap().unwrap_err();
    assert_eq!(error.reason, "reload_unknown_outcome");
    assert_eq!(error.details()["codeVersion"], json!(2));
    let outcomes = error.details()["participants"].as_array().unwrap();
    assert_eq!(outcomes.len(), 2);
    let lost_outcome = outcomes
        .iter()
        .find(|entry| entry["name"] == "lost")
        .unwrap();
    assert_eq!(lost_outcome["outcome"], "unknown");
    assert_eq!(lost_outcome["codeVersion"], 2);
    assert!(lost_outcome["cause"].is_object());
    let healthy_outcome = outcomes
        .iter()
        .find(|entry| entry["name"] == "healthy")
        .unwrap();
    assert_eq!(healthy_outcome["outcome"], "acknowledged");
    assert_eq!(healthy_outcome["codeVersion"], 2);
    drop(subscription);
    parent.reference.release();
    // All three installations remain committed after an unknown acknowledgement.
    lost.reference.release();
    healthy.reference.release();
    drive(&driver, async {
        futures::try_join!(parent.host.close(), lost.host.close(), healthy.host.close()).map(|_| ())
    });
}

fn read(driver: &ManualDriver, reference: &Reference) -> Value {
    drive(driver, reference.call_json("read", None, None)).unwrap()
}

#[test]
fn equal_epoch_child_joining_during_vote_is_fenced_until_current_deployment_catchup() {
    let driver = ManualDriver::new();
    let parent = Node::new(&driver, "parent", "11111111111111111111111111111111");
    let existing = Node::new(&driver, "existing", "22222222222222222222222222222222");
    let late = Node::new(&driver, "late", "33333333333333333333333333333333");
    let (release, waiting) = oneshot::channel();
    let waiting = Rc::new(RefCell::new(Some(waiting)));
    let loader_entered = Rc::new(Cell::new(false));
    let (entered, receiver, token) = (
        loader_entered.clone(),
        waiting.clone(),
        existing.contract.clone(),
    );
    existing
        .host
        .set_reload_loader(Rc::new(move |_| {
            entered.set(true);
            let (wait, contract) = (receiver.borrow_mut().take().unwrap(), token.clone());
            async move {
                wait.await.unwrap();
                Ok(facet(contract, 22))
            }
            .boxed_local()
        }))
        .unwrap();
    let late_loads = Rc::new(Cell::new(0));
    let (loads, contract) = (late_loads.clone(), late.contract.clone());
    late.host
        .set_reload_loader(Rc::new(move |_| {
            loads.set(loads.get() + 1);
            let contract = contract.clone();
            async move { Ok(facet(contract, 22)) }.boxed_local()
        }))
        .unwrap();
    let original = connect(&driver, &parent, &existing);
    drive(&driver, async {
        futures::try_join!(original.0.ready(), original.1.ready()).map(|_| ())
    });
    let transaction = parent.host.reload_cluster(
        vec![facet(parent.contract.clone(), 22)],
        vec![RemoteFacet::new("reviewed", "ready:2").unwrap()],
        ReloadOptions::default(),
    );
    let (report_sender, report) = oneshot::channel();
    driver.spawn(
        async move {
            assert!(report_sender.send(transaction.await).is_ok());
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(
        loader_entered.get(),
        "the original participant is actually loading before the join"
    );
    assert_eq!(read(&driver, &parent.reference)["epoch"], json!(1));
    let newcomer = connect(&driver, &parent, &late);
    let (ready_sender, ready) = oneshot::channel();
    let ready_settled = Rc::new(Cell::new(false));
    let (a, b, settled) = (
        newcomer.0.clone(),
        newcomer.1.clone(),
        ready_settled.clone(),
    );
    driver.spawn(
        async move {
            let outcome = futures::try_join!(a.ready(), b.ready()).map(|_| ());
            settled.set(true);
            assert!(ready_sender.send(outcome).is_ok());
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(
        !ready_settled.get(),
        "equal-epoch hello during a parent vote must remain fenced until that decision"
    );
    assert_eq!(
        late_loads.get(),
        0,
        "catch-up waits for the selected deployment basis"
    );
    release.send(()).unwrap();
    let report = drive(&driver, async { report.await.unwrap() });
    assert_eq!(report.local().current_version(), 2);
    assert_eq!(
        report.children().len(),
        1,
        "the joining peer was not an original voter"
    );
    drive(&driver, async { ready.await.unwrap() });
    assert_eq!(late_loads.get(), 1);
    assert_eq!(
        read(&driver, &late.reference),
        json!({"marker":22,"epoch":2})
    );
    parent.host.set_route("late", newcomer.0).unwrap();
    let routed = drive(
        &driver,
        parent
            .host
            .get(late.contract.clone(), json!({}), GetOptions::default()),
    );
    assert_eq!(read(&driver, &routed), json!({"marker":22,"epoch":2}));
    routed.release();
    parent.reference.release();
    existing.reference.release();
    late.reference.release();
    drive(&driver, async {
        futures::try_join!(
            parent.host.close(),
            existing.host.close(),
            late.host.close()
        )
        .map(|_| ())
    });
}

#[test]
fn parent_close_during_actual_catchup_cancels_admission_but_child_close_joins_its_loader() {
    let driver = ManualDriver::new();
    let parent = Node::new(&driver, "parent", "66666666666666666666666666666666");
    let child = Node::new(&driver, "child", "77777777777777777777777777777777");
    drive(
        &driver,
        parent.host.reload_cluster(
            vec![facet(parent.contract.clone(), 22)],
            vec![RemoteFacet::new("reviewed", "ready:2").unwrap()],
            ReloadOptions::default(),
        ),
    );
    let (release, waiting) = oneshot::channel();
    let receiver = Rc::new(RefCell::new(Some(waiting)));
    let entered = Rc::new(Cell::new(false));
    let finished = Rc::new(Cell::new(false));
    let (wait, started, settled, token) = (
        receiver.clone(),
        entered.clone(),
        finished.clone(),
        child.contract.clone(),
    );
    child
        .host
        .set_reload_loader(Rc::new(move |_| {
            started.set(true);
            let (wait, settled, token) = (
                wait.borrow_mut().take().unwrap(),
                settled.clone(),
                token.clone(),
            );
            async move {
                wait.await.unwrap();
                settled.set(true);
                Ok(facet(token, 22))
            }
            .boxed_local()
        }))
        .unwrap();
    let connection = connect(&driver, &parent, &child);
    let ready = Rc::new(RefCell::new(None));
    let (a, b, observed) = (connection.0.clone(), connection.1.clone(), ready.clone());
    driver.spawn(
        async move {
            *observed.borrow_mut() = Some(futures::try_join!(a.ready(), b.ready()).map(|_| ()));
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(
        entered.get(),
        "the fenced peer is actually loading the remembered deployment"
    );
    assert!(ready.borrow().is_none());
    let parent_closed = Rc::new(RefCell::new(None));
    let (closing, observed) = (parent.host.clone(), parent_closed.clone());
    driver.spawn(
        async move {
            *observed.borrow_mut() = Some(closing.close().await);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(
        parent_closed
            .borrow_mut()
            .take()
            .expect("parent cancels its catch-up waiter")
            .is_ok()
    );
    assert!(
        ready
            .borrow_mut()
            .take()
            .expect("fenced ready observer is refused")
            .is_err()
    );
    assert!(
        !finished.get(),
        "remote physical loading is independent of parent close"
    );
    assert_eq!(
        read(&driver, &child.reference),
        json!({"marker":11,"epoch":1})
    );
    let child_closed = Rc::new(RefCell::new(None));
    let (closing, observed) = (child.host.clone(), child_closed.clone());
    driver.spawn(
        async move {
            *observed.borrow_mut() = Some(closing.close().await);
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert!(child_closed.borrow().is_none());
    release.send(()).unwrap();
    driver.run_until_stalled();
    assert!(finished.get());
    assert!(
        child_closed
            .borrow_mut()
            .take()
            .expect("child joins its actual loader")
            .is_ok()
    );
    parent.reference.release();
    child.reference.release();
}

#[test]
fn failed_catchup_keeps_both_installations_and_reconnect_can_load_the_remembered_basis() {
    let driver = ManualDriver::new();
    let parent = Node::new(&driver, "parent", "88888888888888888888888888888888");
    let child = Node::new(&driver, "child", "99999999999999999999999999999999");
    drive(
        &driver,
        parent.host.reload_cluster(
            vec![facet(parent.contract.clone(), 22)],
            vec![RemoteFacet::new("reviewed", "ready:2").unwrap()],
            ReloadOptions::default(),
        ),
    );
    child
        .host
        .set_reload_loader(Rc::new(|_| {
            async {
                Err(RuntimeError::invalid(
                    "fixture_loader",
                    "embedding refused the artifact",
                ))
            }
            .boxed_local()
        }))
        .unwrap();
    let rejected = connect(&driver, &parent, &child);
    let result = drive(&driver, async {
        Ok(futures::try_join!(rejected.0.ready(), rejected.1.ready()).map(|_| ()))
    });
    assert_eq!(result.unwrap_err().reason, "fixture_loader");
    driver.run_until_stalled();
    assert_eq!(
        read(&driver, &parent.reference),
        json!({"marker":22,"epoch":2})
    );
    assert_eq!(
        read(&driver, &child.reference),
        json!({"marker":11,"epoch":1})
    );
    let loads = Rc::new(Cell::new(0));
    let (loaded, token) = (loads.clone(), child.contract.clone());
    child
        .host
        .set_reload_loader(Rc::new(move |artifact| {
            assert_eq!(artifact.artifact(), "ready:2");
            loaded.set(loaded.get() + 1);
            let token = token.clone();
            async move { Ok(facet(token, 22)) }.boxed_local()
        }))
        .unwrap();
    let retried = connect(&driver, &parent, &child);
    drive(&driver, async {
        futures::try_join!(retried.0.ready(), retried.1.ready()).map(|_| ())
    });
    assert_eq!(loads.get(), 1);
    assert_eq!(
        read(&driver, &child.reference),
        json!({"marker":22,"epoch":2})
    );
    parent.reference.release();
    child.reference.release();
    drive(&driver, async {
        futures::try_join!(parent.host.close(), child.host.close()).map(|_| ())
    });
}
