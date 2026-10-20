use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use std::{
    cell::{Cell as Counter, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};
use tembra_core::{
    contracts::{Contract, Member, Placement},
    rpc::*,
    *,
};

#[path = "consumer_linger/support.rs"]
mod support;
use support::*;
#[path = "consumer_linger/connection_quota.rs"]
mod connection_quota;

#[test]
fn p03_01_policy_edges_and_default_zero_release_immediately() {
    assert_eq!(ConsumerLingerOptions::default().capacity(), 4);
    assert!(ConsumerLingerOptions::new(BTreeMap::new(), 1025).is_err());
    assert!(ConsumerLingerOptions::new(BTreeMap::from([("".into(), ms(1))]), 1).is_err());
    assert!(
        ConsumerLingerOptions::new(BTreeMap::from([(NAME.into(), Duration::from_nanos(1))]), 1)
            .is_err()
    );
    assert!(ConsumerLingerOptions::new(BTreeMap::from([(NAME.into(), Duration::MAX)]), 1).is_err());
    for options in [
        ConsumerLingerOptions::default(),
        policy(0, 4),
        policy(40, 0),
        ConsumerLingerOptions::new(BTreeMap::from([("other".into(), ms(40))]), 4).unwrap(),
    ] {
        let f = Fixture::new(options, 8, 0);
        f.get("a", true).release();
        f.driver.run_until_stalled();
        assert_eq!(f.count("release"), 1);
        assert_eq!(&*f.closed.borrow(), &["a"]);
        f.close();
    }
}

#[test]
fn p03_02_warm_full_reuses_one_hold_and_hydration() {
    let f = Fixture::new(policy(40, 4), 8, 0);
    let first = f.get("a", true);
    let snapshots = f.incoming("snapshot");
    first.release();
    f.driver.run_until_stalled();
    for index in 0..6 {
        let next = f
            .driver
            .run_until(f.client.get(
                f.token.clone(),
                json!({"unused":index,"id":"a"}),
                GetOptions::default(),
            ))
            .unwrap();
        assert_eq!(
            next.readable("count").unwrap().snapshot().unwrap().value(),
            &json!(0)
        );
        next.release();
        f.driver.run_until_stalled();
    }
    assert_eq!(f.count("hold"), 1);
    assert_eq!(f.incoming("snapshot"), snapshots);
    assert_eq!(f.count("release"), 0);
    assert_eq!(f.count("unsubscribe"), 0);
    f.advance(40);
    assert_eq!(f.count("release"), 1);
    f.close();
}

#[test]
fn p03_03_idle_replicas_apply_and_ack_without_reviving_observers() {
    let f = Fixture::new(policy(40, 4), 8, 0);
    let first = f.get("a", true);
    let values = Rc::new(RefCell::new(Vec::new()));
    let saved = values.clone();
    let old = first.readable("count").unwrap();
    let _subscription = old
        .subscribe(move |snapshot| {
            saved.borrow_mut().push(snapshot.value().clone());
            Ok(())
        })
        .unwrap();
    first.release();
    let ack = f.count("state_ack");
    f.write("a", 9);
    assert!(f.count("state_ack") > ack);
    assert_eq!(&*values.borrow(), &[json!(0)]);
    assert_eq!(old.snapshot().unwrap_err().code, ErrorCode::Released);
    let next = f.get("a", true);
    assert_eq!(
        next.readable("count").unwrap().snapshot().unwrap().value(),
        &json!(9)
    );
    assert_eq!(f.count("hold"), 1);
    f.close();
}

#[test]
fn p03_04_method_only_idle_stays_off_until_full_acquisition() {
    let f = Fixture::new(policy(40, 4), 8, 0);
    f.get("a", false).release();
    f.driver.run_until_stalled();
    f.write("a", 7);
    assert_eq!(f.incoming("snapshot"), 0);
    let next = f.get("a", true);
    assert_eq!(
        next.readable("count").unwrap().snapshot().unwrap().value(),
        &json!(7)
    );
    assert_eq!(f.count("subscribe"), 1);
    assert_eq!(f.count("hold"), 1);
    f.close();
}

#[test]
fn p03_05_deadlines_and_cancelled_timer_do_not_close_later_intervals() {
    let f = Fixture::new(policy(40, 4), 8, 0);
    f.get("a", true).release();
    f.driver.run_until_stalled();
    f.advance(39);
    let next = f.get("a", true);
    f.advance(1); // The previous interval's timer cannot release this live claim.
    assert_eq!(f.count("release"), 0);
    next.release();
    f.driver.run_until_stalled();
    f.advance(39);
    assert_eq!(f.count("release"), 0);
    f.driver.advance(ms(1)); // Timer woke but has not polled; lookup must check time itself.
    let fresh = f
        .client
        .hold(f.token.clone(), json!({"id":"a"}), GetOptions::default())
        .unwrap();
    f.driver.run_until(fresh.ready()).unwrap();
    assert_eq!(f.count("hold"), 2);
    assert_eq!(f.count("release"), 1);
    f.close();
}

#[test]
fn p03_06_lru_uses_last_release_and_never_evicts_active_holds() {
    let f = Fixture::new(policy(100, 2), 8, 0);
    let active = f.get("active", true);
    f.get("a", true).release();
    f.get("b", true).release();
    f.get("a", true).release();
    f.write("b", 3); // State activity does not refresh release order.
    f.get("c", true).release();
    f.driver.run_until_stalled();
    assert_eq!(&*f.closed.borrow(), &["b"]);
    assert_eq!(active.status(), RefStatus::Live);
    assert_eq!(f.count("release"), 1);
    f.close();
}

#[test]
fn p03_07_quota_reuses_then_evicts_idle_before_refusing_all_active() {
    let f = Fixture::new(policy(100, 4), 1, 0);
    f.get("a", true).release();
    let again = f.get("a", true);
    assert_eq!(f.count("hold"), 1);
    let Err(error) = f
        .client
        .hold(f.token.clone(), json!({"id":"b"}), GetOptions::default())
    else {
        panic!("all-active quota accepted another hold");
    };
    assert_eq!(error.reason, "too_many_holds");
    assert_eq!(again.status(), RefStatus::Live);
    again.release();
    let b = f.get("b", true);
    assert_eq!(b.status(), RefStatus::Live);
    assert_eq!(&*f.closed.borrow(), &["a"]);
    f.close();
}

#[test]
fn p03_08_first_loading_release_does_not_warm_and_policy_failure_forgets_idle() {
    let f = Fixture::new(policy(100, 4), 1, 0);
    let pending = f
        .client
        .hold(f.token.clone(), json!({"id":"a"}), GetOptions::default())
        .unwrap();
    pending.release();
    f.driver.run_until_stalled();
    assert_eq!(f.count("hold"), 0);
    f.get("b", true).release();
    f.driver.run_until_stalled();
    f.outbound.close(CloseReason::new(RuntimeError::new(
        ErrorCode::Forbidden,
        "policy",
        "policy denied",
    )));
    f.driver.run_until_stalled();
    // A terminal idle record must not consume the only Host hold budget.
    assert!(
        f.client
            .hold(f.token.clone(), json!({"id":"c"}), GetOptions::default())
            .is_ok()
    );
    f.close();
}

#[test]
fn p03_09_released_clones_and_observers_do_not_own_idle_authority() {
    let f = Fixture::new(policy(40, 4), 8, 0);
    let first = f.get("a", true);
    let retained = first.clone();
    let statuses = Rc::new(RefCell::new(Vec::new()));
    let seen = statuses.clone();
    let _remove = retained.on_status(move |status, _| seen.borrow_mut().push(status));
    first.release();
    assert_eq!(retained.status(), RefStatus::Released);
    assert_eq!(
        f.driver
            .run_until(retained.call_json("noop", None, None))
            .unwrap_err()
            .code,
        ErrorCode::Released
    );
    f.get("a", true).release();
    f.advance(40);
    assert_eq!(&*statuses.borrow(), &[RefStatus::Released]);
    assert_eq!(&*f.closed.borrow(), &["a"]);
    f.close();
}

#[test]
fn p03_10_provider_linger_starts_at_wire_release_not_logical_release() {
    let f = Fixture::new(policy(40, 4), 8, 100);
    f.get("a", true).release();
    f.driver.run_until_stalled();
    f.advance(39);
    assert_eq!(f.count("release"), 0);
    f.advance(1);
    assert_eq!(f.count("release"), 1);
    f.advance(99);
    assert!(f.closed.borrow().is_empty());
    f.advance(1);
    assert_eq!(&*f.closed.borrow(), &["a"]);
    f.close();
}

#[test]
fn p03_11_distinct_consumer_fingerprints_never_share_idle_authority() {
    let f = Fixture::new(policy(100, 4), 8, 0);
    f.get("a", true).release();
    let reduced = Rc::new(
        Contract::remote(
            NAME,
            1,
            f.token.input().clone(),
            Placement::Named {
                name: "server".into(),
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
    let next = f
        .driver
        .run_until(
            f.client
                .get(reduced, json!({"id":"a"}), GetOptions::default()),
        )
        .unwrap();
    assert_eq!(next.status(), RefStatus::Live);
    assert_eq!(f.count("hold"), 2);
    f.close();
}

#[test]
fn p03_12_reconnect_reholds_applied_idle_without_extending_deadline() {
    let f = Fixture::new(policy(40, 4), 8, 0);
    f.get("a", true).release();
    f.driver.run_until_stalled();
    f.advance(10);
    f.outbound.close(CloseReason::closed());
    f.driver.run_until_stalled();
    let (left, right, _) = memory_link(f.driver.clone(), MemoryLinkOptions::default()).unwrap();
    let route = f
        .client
        .connect_client(
            counted(left, &f.sent),
            "server",
            ConnectionOptions::default(),
        )
        .unwrap();
    let _accepted = f
        .server
        .accept_client(
            counted(right, &f.received),
            json!("accepted-user"),
            ConnectionOptions::default(),
        )
        .unwrap();
    f.client.set_route("server", route).unwrap();
    f.driver.run_until_stalled();
    assert_eq!(f.count("hold"), 2);
    f.advance(29);
    assert_eq!(f.count("release"), 0);
    f.advance(1);
    assert_eq!(f.count("release"), 1);
    f.close();
}

#[test]
fn p03_13_close_drains_idle_without_waiting_ttl() {
    let f = Fixture::new(policy(100_000, 4), 8, 0);
    f.get("a", true).release();
    f.driver.run_until_stalled();
    let before = f.driver.now();
    f.close();
    assert_eq!(f.driver.now(), before);
    assert_eq!(&*f.closed.borrow(), &["a"]);
}

#[test]
fn p03_14_repeated_cycles_stay_bounded() {
    let f = Fixture::new(policy(100, 2), 8, 0);
    for index in 0_usize..80 {
        f.get(&index.to_string(), true).release();
        f.driver.run_until_stalled();
        assert_eq!(f.closed.borrow().len(), (index + 1).saturating_sub(2));
    }
    assert_eq!(f.count("hold") - f.count("release"), 2);
    f.advance(100);
    assert_eq!(f.closed.borrow().len(), 80);
    f.close();
}

#[test]
fn p03_15_release_observer_reacquires_before_last_claim_decision() {
    let f = Fixture::new(policy(40, 1), 8, 0);
    let first = f.get("a", true);
    let revived = Rc::new(RefCell::new(None));
    let (save, client, token) = (revived.clone(), f.client.clone(), f.token.clone());
    let _remove = first.on_status(move |status, _| {
        if status == RefStatus::Released {
            *save.borrow_mut() = Some(
                client
                    .hold(token.clone(), json!({"id":"a"}), GetOptions::default())
                    .unwrap(),
            );
        }
    });
    first.release();
    f.driver.run_until_stalled();
    let next = revived.borrow_mut().take().unwrap();
    f.driver.run_until(next.ready()).unwrap();
    f.advance(40);
    assert_eq!(next.status(), RefStatus::Live);
    assert_eq!(f.count("hold"), 1);
    assert_eq!(f.count("release"), 0);
    next.release();
    f.advance(40);
    assert_eq!(f.count("release"), 1);
    f.close();
}

#[test]
fn p03_10_expiry_does_not_release_a_physically_running_provider_method() {
    let f = Fixture::new(policy(40, 4), 8, 0);
    let reference = f.get("a", true);
    let (finish, receive) = oneshot::channel();
    *f.wait.borrow_mut() = Some(receive);
    let mut call = reference.call_json("wait", None, None).boxed_local();
    assert!((&mut call).now_or_never().is_none());
    f.driver.run_until_stalled();
    assert!(f.started.get());
    reference.release();
    f.advance(40);
    assert_eq!(f.count("release"), 1);
    assert!(
        f.closed.borrow().is_empty(),
        "actual provider invocation must pin its resource"
    );
    finish.send(()).unwrap();
    f.driver.run_until_stalled();
    assert_eq!(&*f.closed.borrow(), &["a"]);
    drop(call);
    f.close();
}

#[test]
fn p03_12_expiry_during_pending_reconnect_cannot_emit_a_late_hold() {
    let f = Fixture::new(policy(40, 4), 8, 0);
    f.get("a", true).release();
    f.driver.run_until_stalled();
    f.outbound.close(CloseReason::closed());
    f.driver.run_until_stalled();
    let (left, right, _) = memory_link(f.driver.clone(), MemoryLinkOptions::default()).unwrap();
    let route = f
        .client
        .connect_client(
            counted(left, &f.sent),
            "server",
            ConnectionOptions::default(),
        )
        .unwrap();
    let _accepted = f
        .server
        .accept_client(
            counted(right, &f.received),
            json!("accepted-user"),
            ConnectionOptions::default(),
        )
        .unwrap();
    let (finish, receive) = oneshot::channel();
    let pending = Rc::new(RefCell::new(Some(receive)));
    f.client
        .set_resolver(Rc::new(move |_| {
            let receive = pending.borrow_mut().take().unwrap();
            let route = route.clone();
            async move {
                receive.await.unwrap();
                Ok(route)
            }
            .boxed_local()
        }))
        .unwrap();
    f.driver.run_until_stalled();
    f.advance(40);
    finish.send(()).unwrap();
    f.driver.run_until_stalled();
    assert_eq!(
        f.count("hold"),
        1,
        "expired idle resolver completion has no ownership"
    );
    assert!(f.client.errors().is_empty());
    f.close();
}

#[test]
fn p03_13_idle_drain_preserves_actual_held_cleanup_dependencies() {
    let f = Fixture::new(policy(100_000, 4), 8, 0);
    f.get("idle", true).release();
    f.driver.run_until_stalled();
    let owner = Rc::new(Contract::local("cleanup-owner", 1, json!({}), BTreeMap::new()).unwrap());
    let token = f.token.clone();
    let sent = f.sent.clone();
    let closed = f.closed.clone();
    let completed = Rc::new(Counter::new(false));
    let success = completed.clone();
    f.client
        .serve(
            owner.clone(),
            Rc::new(move |_, _, declaration| {
                let dependency =
                    declaration.get(token.clone(), json!({"id":"held"}), GetOptions::default())?;
                let flush = dependency.clone();
                let closed = closed.clone();
                let sent = sent.clone();
                let success = success.clone();
                let mut resource = ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local())
                    .with_close(move |_| {
                        assert!(!closed.borrow().iter().any(|id| id == "held"));
                        assert!(sent.borrow().iter().any(|frame| frame["t"] == "release"));
                        let (flush, success) = (flush.clone(), success.clone());
                        async move {
                            flush.call_json("noop", None, None).await?;
                            success.set(true);
                            Ok(())
                        }
                        .boxed_local()
                    });
                resource
                    .deps
                    .push(ResourceDependency::Reference(dependency));
                declaration.resource("cleanup", resource)?;
                Ok(Implementation::new())
            }),
            ServeOptions::default(),
        )
        .unwrap();
    let _held = f
        .driver
        .run_until(f.client.get(owner, json!({}), GetOptions::default()))
        .unwrap();
    f.close();
    assert!(completed.get());
    assert_eq!(f.inbound.status(), ConnectionStatus::Closed);
}

#[test]
fn p03_01_clock_overflow_disables_optional_retention_without_panicking() {
    let f = Fixture::new(policy(40, 4), 8, 0);
    let reference = f.get("a", true);
    f.driver.advance(Duration::MAX);
    reference.release();
    f.driver.run_until_stalled();
    assert_eq!(f.count("release"), 1);
    assert_eq!(&*f.closed.borrow(), &["a"]);
    f.close();
}

#[test]
fn p03_11_forwarded_idle_grant_is_evicted_by_actual_peer_revocation_and_source_close() {
    for revoke in [true, false] {
        let f = Fixture::new(policy(100, 4), 8, 0);
        let (parent, authority, source) = forwarding_parent(&f);
        let reference = f
            .driver
            .run_until(parent.get(f.token.clone(), json!({"id":"a"}), GetOptions::default()))
            .unwrap();
        let forwarded = f
            .sent
            .borrow()
            .iter()
            .find(|frame| frame["t"] == "forward_hold")
            .unwrap()
            .clone();
        assert_eq!(forwarded["caller"]["principal"], json!("alice"));
        reference.release();
        f.driver.run_until_stalled();
        assert!(
            f.closed.borrow().is_empty(),
            "middle consumer retained the forwarded hold"
        );
        if revoke {
            let frame = json!({"t":"invalidate_principal", "principal":"alice",
                "origin":forwarded["caller"]["origin"], "through_generation":0});
            authority
                .try_send(Rc::from(serde_json::to_vec(&frame).unwrap()))
                .unwrap();
        } else {
            source.close(CloseReason::closed());
        }
        f.driver.run_until_stalled();
        assert_eq!(&*f.closed.borrow(), &["a"]);
        if revoke {
            assert_eq!(source.status(), ConnectionStatus::Open);
            let refused = parent
                .hold(f.token.clone(), json!({"id":"a"}), GetOptions::default())
                .unwrap();
            let error = f.driver.run_until(refused.ready()).unwrap_err();
            assert_eq!(error.code, ErrorCode::Forbidden);
            assert_eq!(
                f.sent
                    .borrow()
                    .iter()
                    .filter(|frame| frame["t"] == "forward_hold")
                    .count(),
                1,
                "the revoked idle grant cannot be revived by a fresh caller claim"
            );
        }
        f.driver.run_until(parent.close()).unwrap();
        f.close();
    }
}

#[test]
fn p03_15_eviction_release_reentry_cannot_remove_the_new_same_address_binding() {
    let f = Fixture::new(policy(40, 1), 8, 0);
    f.get("a", true).release();
    f.driver.run_until_stalled();
    let reacquired = Rc::new(RefCell::new(None));
    let (save, client, token) = (reacquired.clone(), f.client.clone(), f.token.clone());
    *f.on_release.borrow_mut() = Some(Box::new(move || {
        *save.borrow_mut() = Some(
            client
                .hold(token.clone(), json!({"id":"a"}), GetOptions::default())
                .unwrap(),
        );
    }));
    f.get("b", true).release();
    f.driver.run_until_stalled();
    let next = reacquired.borrow_mut().take().unwrap();
    f.driver.run_until(next.ready()).unwrap();
    f.advance(40);
    assert_eq!(next.status(), RefStatus::Live);
    assert_eq!(f.count("hold"), 3);
    assert_eq!(f.count("release"), 2);
    f.close();
}

#[test]
fn p03_11_principal_and_origin_partitions_keep_distinct_idle_holds() {
    let f = Fixture::new(policy(100, 4), 8, 0);
    let (parent, _, _) = forwarding_parent(&f);
    parent.set_route_authorizer(Rc::new(|_| Ok(()))).unwrap();
    let mut frontends = Vec::new();
    for principal in ["alice", "bob", "alice"] {
        let frontend = Host::new(
            HostOptions::new("frontend", json!("self")),
            f.driver.clone(),
        );
        let (left, right, _) = memory_link(f.driver.clone(), MemoryLinkOptions::default()).unwrap();
        let route = frontend
            .connect_client(left, "parent", ConnectionOptions::default())
            .unwrap();
        let _incoming = parent
            .accept_client(right, json!(principal), ConnectionOptions::default())
            .unwrap();
        frontend.set_route("server", route).unwrap();
        let view = f
            .driver
            .run_until(frontend.get(
                f.token.clone(),
                json!({"id":"partition"}),
                GetOptions::default(),
            ))
            .unwrap();
        view.release();
        f.driver.run_until_stalled();
        frontends.push(frontend);
    }
    let partitions: Vec<_> = f
        .sent
        .borrow()
        .iter()
        .filter(|frame| frame["t"] == "forward_hold")
        .map(|frame| frame["caller"].clone())
        .collect();
    assert_eq!(partitions.len(), 3);
    assert_eq!(partitions[0]["principal"], json!("alice"));
    assert_eq!(partitions[1]["principal"], json!("bob"));
    assert_eq!(partitions[2]["principal"], json!("alice"));
    assert_ne!(partitions[0]["origin"], partitions[2]["origin"]);
    assert_eq!(
        f.count("release"),
        0,
        "all three independently authorized holds remain idle"
    );
    for frontend in frontends {
        f.driver.run_until(frontend.close()).unwrap();
    }
    f.driver.run_until(parent.close()).unwrap();
    f.close();
}
