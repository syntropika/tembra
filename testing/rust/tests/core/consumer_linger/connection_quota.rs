use super::*;
use std::collections::BTreeSet;

fn fresh_connection(f: &Fixture, limit: usize) -> (Connection, Rc<RefCell<Vec<Value>>>) {
    let frames = Rc::new(RefCell::new(Vec::new()));
    let (left, right, _) = memory_link(f.driver.clone(), MemoryLinkOptions::default()).unwrap();
    let mut options = ConnectionOptions::default();
    options.limits.max_holds = limit;
    let outgoing = f
        .client
        .connect_client(counted(left, &frames), "server", options)
        .unwrap();
    let _incoming = f
        .server
        .accept_client(right, json!("accepted-user"), ConnectionOptions::default())
        .unwrap();
    f.driver.run_until(outgoing.ready()).unwrap();
    (outgoing, frames)
}
fn new_route(f: &Fixture, limit: usize) -> Rc<RefCell<Vec<Value>>> {
    let (outgoing, frames) = fresh_connection(f, limit);
    f.client.set_route("server", outgoing).unwrap();
    f.driver.run_until_stalled();
    frames
}
fn open_ids(frames: &[Value]) -> BTreeSet<String> {
    let mut open = BTreeMap::new();
    for frame in frames {
        if frame["t"] == "hold" {
            open.insert(
                frame["hold"].as_u64().unwrap(),
                frame["input"]["id"].as_str().unwrap(),
            );
        } else if frame["t"] == "release" {
            open.remove(&frame["hold"].as_u64().unwrap());
        }
    }
    open.into_values().map(str::to_owned).collect()
}
#[test]
fn connection_budget_evicts_oldest_idle_before_new_real_hold() {
    let f = Fixture::with_connection_limit(policy(60_000, 4), 16, 0, 2);
    f.get("idle-a", true).release();
    f.get("idle-b", true).release();
    let real = f.get("real", true);
    assert_eq!(f.count("hold"), 3);
    assert_eq!(f.count("release"), 1);
    f.driver.run_until_stalled();
    assert_eq!(&*f.closed.borrow(), &["idle-a"]);
    assert_eq!(
        open_ids(&f.sent.borrow()),
        BTreeSet::from(["idle-b".into(), "real".into()])
    );
    real.release();
    f.close();
}
#[test]
fn connection_eviction_preserves_other_route_idle_and_same_route_live_holds() {
    let f = Fixture::with_connection_limit(policy(60_000, 4), 16, 0, 2);
    let other = Host::new(HostOptions::new("other", json!(null)), f.driver.clone());
    other
        .set_rpc_identity(RootOrigin::new("other", "22222222222222222222222222222222").unwrap())
        .unwrap();
    let token = Rc::new(
        Contract::remote(
            NAME,
            1,
            json!({}),
            Placement::Named {
                name: "other".into(),
            },
            BTreeMap::from([(
                "default".into(),
                BTreeMap::from([(
                    "noop".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                )]),
            )]),
        )
        .unwrap(),
    );
    let closes = Rc::new(Counter::new(0));
    let closed = closes.clone();
    other
        .serve(
            token.clone(),
            Rc::new(move |_, _, declaration| {
                let closed = closed.clone();
                declaration.resource(
                    "lifetime",
                    ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                        move |_| {
                            closed.set(closed.get() + 1);
                            async { Ok(()) }.boxed_local()
                        },
                    ),
                )?;
                Ok(Implementation::new()
                    .variant("default")
                    .json_method("noop", |_, _| async { Ok(None) }.boxed_local()))
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let frames = Rc::new(RefCell::new(Vec::new()));
    let (left, right, _) = memory_link(f.driver.clone(), MemoryLinkOptions::default()).unwrap();
    let outgoing = f
        .client
        .connect_client(
            counted(left, &frames),
            "other",
            ConnectionOptions::default(),
        )
        .unwrap();
    let _incoming = other
        .accept_client(right, json!(null), ConnectionOptions::default())
        .unwrap();
    f.client.set_route("other", outgoing).unwrap();
    let old_other = f
        .driver
        .run_until(
            f.client
                .get(token.clone(), json!({}), GetOptions::default()),
        )
        .unwrap();
    let other_id = old_other.object_id();
    old_other.release(); // Globally oldest, but not attached to the pressured connection.
    f.get("idle-a", true).release();
    let active = f.get("active-a", true);
    let new = f.get("new-a", true);
    f.driver.run_until_stalled();
    assert_eq!(&*f.closed.borrow(), &["idle-a"]);
    assert_eq!(closes.get(), 0);
    assert_eq!(active.status(), RefStatus::Live);
    assert_eq!(
        f.driver
            .run_until(active.call_json("noop", None, None))
            .unwrap(),
        None
    );
    let warm = f
        .driver
        .run_until(f.client.get(token, json!({}), GetOptions::default()))
        .unwrap();
    assert_eq!(warm.object_id(), other_id);
    assert_eq!(
        frames.borrow().iter().filter(|v| v["t"] == "hold").count(),
        1
    );
    assert_eq!(
        frames
            .borrow()
            .iter()
            .filter(|v| v["t"] == "release")
            .count(),
        0
    );
    warm.release();
    new.release();
    active.release();
    f.close();
    f.driver.run_until(other.close()).unwrap();
    assert_eq!(closes.get(), 1);
}
#[test]
fn reconnect_real_claims_take_capacity_before_optional_idle_reholds() {
    let f = Fixture::resolver_connection_limit(policy(60_000, 4), 16, 0, 3);
    let a = f.get("active-a", true);
    let b = f.get("active-b", true);
    let old = f.get("idle", true);
    old.release();
    let (connection, frames) = fresh_connection(&f, 2);
    let (release_a, wait_a) = oneshot::channel();
    let (release_b, wait_b) = oneshot::channel();
    let gates = Rc::new(RefCell::new(BTreeMap::from([
        ("active-a".to_owned(), wait_a),
        ("active-b".to_owned(), wait_b),
    ])));
    let waiting = gates.clone();
    f.client
        .set_resolver(Rc::new(move |request| {
            let gate = waiting
                .borrow_mut()
                .remove(request.input["id"].as_str().unwrap());
            let connection = connection.clone();
            async move {
                if let Some(gate) = gate {
                    gate.await.unwrap();
                }
                Ok(connection)
            }
            .boxed_local()
        }))
        .unwrap();
    f.outbound.close(CloseReason::closed());
    f.driver.run_until_stalled();
    assert!(
        gates.borrow().is_empty(),
        "both actual active resolutions are physically suspended"
    );
    assert_eq!(
        open_ids(&frames.borrow()),
        BTreeSet::from(["idle".into()]),
        "the optional resolver rehold demonstrably arrived first"
    );
    release_a.send(()).unwrap();
    f.driver.run_until_stalled();
    assert_eq!(a.status(), RefStatus::Live);
    release_b.send(()).unwrap();
    f.driver.run_until_stalled();
    assert_eq!(b.status(), RefStatus::Live);
    assert_eq!(
        open_ids(&frames.borrow()),
        BTreeSet::from(["active-a".into(), "active-b".into()])
    );
    assert_eq!(
        f.driver.run_until(a.call_json("noop", None, None)).unwrap(),
        None
    );
    assert_eq!(old.status(), RefStatus::Released);
    a.release();
    b.release();
    f.close();
}
#[test]
fn idle_reconnect_never_evicts_another_idle_binding_just_to_rehold_itself() {
    let f = Fixture::with_connection_limit(policy(60_000, 4), 16, 0, 2);
    f.get("idle-a", true).release();
    f.get("idle-b", true).release();
    let frames = new_route(&f, 1);
    assert_eq!(
        frames.borrow().iter().filter(|v| v["t"] == "hold").count(),
        1
    );
    assert_eq!(
        frames
            .borrow()
            .iter()
            .filter(|v| v["t"] == "release")
            .count(),
        0
    );
    let ids = open_ids(&frames.borrow());
    let retained = ids.iter().next().unwrap();
    let warm = f.get(retained, true);
    assert_eq!(
        frames.borrow().iter().filter(|v| v["t"] == "hold").count(),
        1
    );
    warm.release();
    f.close();
}
#[test]
fn ordinary_release_keeps_error_accessor_empty_without_reviving_released_views() {
    let driver = ManualDriver::new();
    let local = Host::new(HostOptions::new("local", Value::Null), driver.clone());
    let token = Rc::new(Contract::local("local", 1, json!({}), BTreeMap::new()).unwrap());
    local
        .serve(
            token.clone(),
            Rc::new(|_, _, _| Ok(Implementation::new())),
            ServeOptions::default(),
        )
        .unwrap();
    let reference = driver
        .run_until(local.get(token, json!({}), GetOptions::default()))
        .unwrap();
    reference.release();
    driver.run_until_stalled();
    assert!(
        reference.error().is_none(),
        "ordinary remote release matches the local accessor"
    );
    driver.run_until(local.close()).unwrap();
    for options in [ConsumerLingerOptions::default(), policy(40, 4)] {
        let f = Fixture::new(options, 8, 0);
        let reference = f.get("a", true);
        let clone = reference.clone();
        let readable = reference.readable("count").unwrap();
        assert!(reference.error().is_none());
        reference.release();
        assert_eq!(reference.status(), RefStatus::Released);
        assert_eq!(clone.status(), RefStatus::Released);
        assert!(
            reference.error().is_none(),
            "ordinary release is not a binding failure"
        );
        assert_eq!(readable.snapshot().unwrap_err().code, ErrorCode::Released);
        assert_eq!(
            f.driver
                .run_until(clone.call_json("noop", None, None))
                .unwrap_err()
                .code,
            ErrorCode::Released
        );
        assert_eq!(f.count("call"), 0);
        f.advance(40);
        assert!(
            reference.error().is_none(),
            "TTL expiry must not manufacture a public failure"
        );
        f.close();
        assert!(reference.error().is_none());
    }
}

#[test]
fn actual_binding_failure_still_exposes_its_typed_error() {
    let f = Fixture::new(ConsumerLingerOptions::default(), 8, 0);
    let reference = f.get("failure", true);
    f.outbound.close(CloseReason::new(RuntimeError::new(
        ErrorCode::Forbidden,
        "policy",
        "test policy closure",
    )));
    f.driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Gone);
    let error = reference
        .error()
        .expect("actual failures remain observable");
    assert_eq!(error.code, ErrorCode::Forbidden);
    assert_eq!(error.reason, "policy");
    reference.release();
    assert_eq!(reference.status(), RefStatus::Gone);
    assert_eq!(
        reference.error().unwrap(),
        error,
        "release preserves an existing forced failure"
    );
    f.close();
}

#[test]
fn all_active_connection_capacity_refuses_without_evicting_live_holds() {
    let f = Fixture::with_connection_limit(policy(60_000, 4), 16, 0, 2);
    let a = f.get("active-a", true);
    let b = f.get("active-b", true);
    let error = f
        .driver
        .run_until(f.client.get(
            f.token.clone(),
            json!({"id":"excess"}),
            GetOptions::default(),
        ))
        .err()
        .unwrap();
    assert_eq!(error.reason, "hold_limit");
    assert_eq!(f.count("release"), 0);
    assert_eq!(a.status(), RefStatus::Live);
    assert_eq!(b.status(), RefStatus::Live);
    assert!(f.closed.borrow().is_empty());
    a.release();
    b.release();
    f.close();
}

#[test]
fn connection_eviction_release_reentry_can_release_and_reacquire_without_stale_removal() {
    let f = Fixture::with_connection_limit(policy(60_000, 4), 16, 0, 2);
    f.get("idle-a", true).release();
    let active = f.get("active", true);
    let replaced = Rc::new(RefCell::new(None));
    let (client, token, target, releasing) = (
        f.client.clone(),
        f.token.clone(),
        replaced.clone(),
        active.clone(),
    );
    *f.on_release.borrow_mut() = Some(Box::new(move || {
        releasing.release();
        *target.borrow_mut() = Some(
            client
                .hold(token.clone(), json!({"id":"idle-a"}), GetOptions::default())
                .unwrap(),
        );
    }));
    let demand = f.get("real", true);
    let reentered = replaced.borrow().as_ref().unwrap().clone();
    f.driver.run_until(reentered.ready()).unwrap();
    assert_eq!(active.status(), RefStatus::Released);
    assert_eq!(demand.status(), RefStatus::Live);
    assert_eq!(reentered.status(), RefStatus::Live);
    assert_eq!(
        open_ids(&f.sent.borrow()),
        BTreeSet::from(["idle-a".into(), "real".into()])
    );
    assert_eq!(f.count("release"), 2);
    reentered.release();
    demand.release();
    f.close();
}
