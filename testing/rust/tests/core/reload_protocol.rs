use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use std::{cell::Cell, rc::Rc};
use tembra_core::rpc::{
    ConnectionOptions, ConnectionStatus, Link, LinkEvent, RootOrigin, memory_link,
};
use tembra_core::{CancellationToken, ErrorCode, Host, HostOptions, ManualDriver, ReloadOptions};

fn send(link: &Link, frame: &Value) {
    link.tx
        .try_send(Rc::from(serde_json::to_vec(frame).unwrap()))
        .unwrap();
}
fn hello(client: bool) -> Value {
    let peer = if client {
        json!({"role":"client"})
    } else {
        json!({
            "role":"host", "identity":{"host":"peer","incarnation":"22222222222222222222222222222222"},
            "code_version":1
        })
    };
    json!({"t":"hello","hello":{
        "protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,
        "peer":peer,"receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,"state_window_bytes":262144}
    }})
}
fn controls() -> Vec<Value> {
    vec![
        json!({"t":"reload_prepare","transaction":1,"code_version":2,"facets":[],"remove":[],
            "timing":{"gate":1,"retire":1,"task_stop":1},"catchup":false}),
        json!({"t":"reload_vote","transaction":1,"code_version":2,
            "outcome":{"status":"ready","data":{"prepared":0,"gated":0}}}),
        json!({"t":"reload_commit","transaction":1,"code_version":2}),
        json!({"t":"reload_ack","transaction":1,"code_version":2}),
        json!({"t":"reload_abort","transaction":1,
            "error":{"code":"aborted","reason":"test","message":"test abort","details":null}}),
        json!({"t":"reload_retire","code_version":1}),
    ]
}

#[test]
fn every_reload_control_obeys_configured_roles_before_loader_or_epoch_effects() {
    for role in ["client", "provider", "child", "parent"] {
        for frame in controls() {
            let kind = frame["t"].as_str().unwrap();
            let parent_control = matches!(
                kind,
                "reload_prepare" | "reload_commit" | "reload_abort" | "reload_retire"
            );
            if (role == "parent" && parent_control) || (role == "child" && !parent_control) {
                continue;
            }
            let driver = ManualDriver::new();
            let host = Host::new(HostOptions::new("local", json!(null)), driver.clone());
            host.set_rpc_identity(
                RootOrigin::new("local", "11111111111111111111111111111111").unwrap(),
            )
            .unwrap();
            let loads = Rc::new(Cell::new(0));
            let observed = loads.clone();
            host.set_reload_loader(Rc::new(move |_| {
                observed.set(observed.get() + 1);
                async { panic!("forged role cannot reach application loader") }.boxed_local()
            }))
            .unwrap();
            let (raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
            let connection = match role {
                "client" => host.accept_client(
                    incoming,
                    json!("authenticated-client"),
                    ConnectionOptions::default(),
                ),
                "provider" => host.connect_client(incoming, "peer", ConnectionOptions::default()),
                "child" => host.accept_child(
                    incoming,
                    "peer",
                    json!("authenticated-child"),
                    ConnectionOptions::default(),
                ),
                "parent" => host.connect_parent(
                    incoming,
                    "peer",
                    json!("authenticated-parent"),
                    ConnectionOptions::default(),
                ),
                _ => unreachable!(),
            }
            .unwrap();
            send(&raw, &hello(role == "client"));
            driver.run_until(connection.ready()).unwrap();
            send(&raw, &frame);
            let failure = driver.run_until(connection.closed()).error;
            assert_eq!(failure.code, ErrorCode::Forbidden, "{role} accepted {kind}");
            assert_eq!(failure.reason, "rpc_role");
            assert_eq!(connection.status(), ConnectionStatus::Closed);
            assert_eq!(loads.get(), 0);
            driver.run_until(host.close()).unwrap();
        }
    }
}

#[test]
fn trusted_unsolicited_votes_and_acknowledgements_cannot_invent_a_transaction() {
    for frame in controls()
        .into_iter()
        .filter(|frame| matches!(frame["t"].as_str(), Some("reload_vote" | "reload_ack")))
    {
        let driver = ManualDriver::new();
        let host = Host::new(HostOptions::new("local", json!(null)), driver.clone());
        host.set_rpc_identity(
            RootOrigin::new("local", "11111111111111111111111111111111").unwrap(),
        )
        .unwrap();
        let (raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
        let connection = host
            .accept_child(incoming, "peer", json!(null), ConnectionOptions::default())
            .unwrap();
        send(&raw, &hello(false));
        driver.run_until(connection.ready()).unwrap();
        send(&raw, &frame);
        let failure = driver.run_until(connection.closed()).error;
        assert_eq!(failure.code, ErrorCode::Invalid);
        assert_eq!(failure.reason, "reload_phase");
        driver.run_until(host.close()).unwrap();
    }
}

fn next(driver: &ManualDriver, link: &mut Link) -> Value {
    driver.run_until_stalled();
    loop {
        match link.events.next().now_or_never().expect("queued frame") {
            Some(LinkEvent::Message(bytes)) => return serde_json::from_slice(&bytes).unwrap(),
            Some(LinkEvent::Opened | LinkEvent::Writable) => {}
            other => panic!("expected frame, received {other:?}"),
        }
    }
}

#[test]
fn a_ready_vote_already_in_flight_after_abort_preserves_the_trusted_link() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("local", json!(null)), driver.clone());
    host.set_rpc_identity(RootOrigin::new("local", "11111111111111111111111111111111").unwrap())
        .unwrap();
    let (mut raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
    let connection = host
        .accept_child(incoming, "peer", json!(null), ConnectionOptions::default())
        .unwrap();
    send(&raw, &hello(false));
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(&driver, &mut raw)["t"], "hello");
    let cancellation = CancellationToken::new();
    let reload = host.reload_cluster(
        vec![],
        vec![],
        ReloadOptions {
            cancellation: Some(cancellation.clone()),
            ..Default::default()
        },
    );
    let prepare = next(&driver, &mut raw);
    assert_eq!(prepare["t"], "reload_prepare");
    cancellation.cancel("test explicit abort before vote");
    assert!(driver.run_until(reload).is_err());
    assert_eq!(next(&driver, &mut raw)["t"], "reload_abort");
    send(
        &raw,
        &json!({"t":"reload_vote","transaction":prepare["transaction"],
        "code_version":prepare["code_version"],
        "outcome":{"status":"ready","data":{"prepared":0,"gated":0}}}),
    );
    driver.run_until_stalled();
    assert_eq!(
        connection.status(),
        ConnectionStatus::Open,
        "an already sent ready vote can cross the parent abort without closing a healthy link"
    );
    let epoch = driver
        .run_until(host.root(|context| async move { Ok(context.code_version()) }.boxed_local()))
        .unwrap();
    assert_eq!(epoch, 1);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn trusted_abort_is_idempotent_and_unknown_identity_cannot_preempt_a_ready_candidate() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("local", json!(null)), driver.clone());
    host.set_rpc_identity(RootOrigin::new("local", "11111111111111111111111111111111").unwrap())
        .unwrap();
    let (mut raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
    let connection = host
        .connect_parent(incoming, "peer", json!(null), ConnectionOptions::default())
        .unwrap();
    send(&raw, &hello(false));
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(&driver, &mut raw)["t"], "hello");
    send(&raw, &controls()[0]);
    let vote = next(&driver, &mut raw);
    assert_eq!(vote["t"], "reload_vote");
    assert_eq!(vote["outcome"]["status"], "ready");
    let mut unknown = controls()[4].clone();
    unknown["transaction"] = json!(999);
    send(&raw, &unknown);
    send(&raw, &controls()[2]);
    assert_eq!(next(&driver, &mut raw)["t"], "reload_ack");
    send(&raw, &controls()[4]);
    send(&raw, &controls()[4]);
    driver.run_until_stalled();
    assert_eq!(connection.status(), ConnectionStatus::Open);
    let epoch = driver
        .run_until(host.root(|context| async move { Ok(context.code_version()) }.boxed_local()))
        .unwrap();
    assert_eq!(epoch, 2);
    driver.run_until(host.close()).unwrap();
}

#[test]
fn ended_reload_history_refuses_at_capacity_then_a_later_fifo_vote_releases_older_records() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("local", json!(null)), driver.clone());
    host.set_rpc_identity(RootOrigin::new("local", "11111111111111111111111111111111").unwrap())
        .unwrap();
    let (mut raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
    let connection = host
        .accept_child(incoming, "peer", json!(null), ConnectionOptions::default())
        .unwrap();
    send(&raw, &hello(false));
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(&driver, &mut raw)["t"], "hello");
    for transaction in 1..=64 {
        let cancellation = CancellationToken::new();
        let reload = host.reload_cluster(
            vec![],
            vec![],
            ReloadOptions {
                cancellation: Some(cancellation.clone()),
                ..Default::default()
            },
        );
        let prepare = next(&driver, &mut raw);
        assert_eq!(prepare["t"], "reload_prepare");
        assert_eq!(prepare["transaction"], transaction);
        cancellation.cancel("test abort before peer response");
        assert!(driver.run_until(reload).is_err());
        assert_eq!(next(&driver, &mut raw)["t"], "reload_abort");
        drain_aborts(&driver, &mut raw);
    }
    let refused = driver
        .run_until(host.reload_cluster(vec![], vec![], ReloadOptions::default()))
        .unwrap_err();
    assert_eq!(refused.reason, "reload_history");
    assert_eq!(next(&driver, &mut raw)["t"], "reload_abort");
    drain_aborts(&driver, &mut raw);
    send(
        &raw,
        &json!({"t":"reload_vote","transaction":64,"code_version":2,
        "outcome":{"status":"ready","data":{"prepared":0,"gated":0}}}),
    );
    driver.run_until_stalled();
    let cancellation = CancellationToken::new();
    let retried = host.reload_cluster(
        vec![],
        vec![],
        ReloadOptions {
            cancellation: Some(cancellation.clone()),
            ..Default::default()
        },
    );
    assert_eq!(
        next(&driver, &mut raw)["t"],
        "reload_prepare",
        "an exact later FIFO vote must prove older aborted controls were processed"
    );
    cancellation.cancel("test retry cleanup");
    assert!(driver.run_until(retried).is_err());
    driver.run_until(host.close()).unwrap();
}

fn drain_aborts(driver: &ManualDriver, raw: &mut Link) {
    driver.run_until_stalled();
    while let Some(Some(event)) = raw.events.next().now_or_never() {
        match event {
            LinkEvent::Message(bytes) => {
                let frame: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(frame["t"], "reload_abort");
            }
            LinkEvent::Opened | LinkEvent::Writable => {}
            other => panic!("unexpected event {other:?}"),
        }
    }
}

#[test]
fn child_close_aborts_a_ready_candidate_without_waiting_for_the_parent_decision() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("local", json!(null)), driver.clone());
    host.set_rpc_identity(RootOrigin::new("local", "11111111111111111111111111111111").unwrap())
        .unwrap();
    let (mut raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
    let connection = host
        .connect_parent(incoming, "peer", json!(null), ConnectionOptions::default())
        .unwrap();
    send(&raw, &hello(false));
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(&driver, &mut raw)["t"], "hello");
    send(&raw, &controls()[0]);
    assert_eq!(next(&driver, &mut raw)["outcome"]["status"], "ready");
    assert_eq!(connection.status(), ConnectionStatus::Open);
    let mut close = host.close().boxed_local();
    assert!(close.as_mut().now_or_never().is_none());
    driver.run_until_stalled();
    assert!(
        close
            .now_or_never()
            .expect("an undecided Ready candidate must not deadlock Host close")
            .is_ok()
    );
    assert_eq!(connection.status(), ConnectionStatus::Closed);
}

#[test]
fn trusted_membership_cannot_join_after_local_reload_admission() {
    for parent in [false, true] {
        let driver = ManualDriver::new();
        let host = Host::new(HostOptions::new("local", json!(null)), driver.clone());
        host.set_rpc_identity(
            RootOrigin::new("local", "11111111111111111111111111111111").unwrap(),
        )
        .unwrap();
        let reload = host.reload(vec![], ReloadOptions::default());
        let (_raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
        let joined = if parent {
            host.connect_parent(incoming, "peer", json!(null), ConnectionOptions::default())
        } else {
            host.accept_child(incoming, "peer", json!(null), ConnectionOptions::default())
        };
        let error = joined
            .err()
            .expect("trusted membership must refuse while a local reload owns its slot");
        assert_eq!(error.code, ErrorCode::Unavailable);
        assert_eq!(error.reason, "reload_busy");
        assert_eq!(driver.run_until(reload).unwrap().current_version(), 2);
        driver.run_until(host.close()).unwrap();
    }
}

#[test]
fn a_child_refuses_third_prepare_until_parent_retirement_permission_is_granted() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("local", json!(null)), driver.clone());
    host.set_rpc_identity(RootOrigin::new("local", "11111111111111111111111111111111").unwrap())
        .unwrap();
    let (mut raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
    let connection = host
        .connect_parent(incoming, "peer", json!(null), ConnectionOptions::default())
        .unwrap();
    send(&raw, &hello(false));
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(&driver, &mut raw)["t"], "hello");
    send(&raw, &controls()[0]);
    assert_eq!(next(&driver, &mut raw)["outcome"]["status"], "ready");
    send(&raw, &controls()[2]);
    assert_eq!(next(&driver, &mut raw)["t"], "reload_ack");
    let mut third = controls()[0].clone();
    third["transaction"] = json!(2);
    third["code_version"] = json!(3);
    send(&raw, &third);
    let refusal = next(&driver, &mut raw);
    assert_eq!(refusal["outcome"]["status"], "rejected");
    assert_eq!(refusal["outcome"]["data"]["code"], "forbidden");
    assert_eq!(refusal["outcome"]["data"]["reason"], "reload_retire");
    let epoch = driver
        .run_until(host.root(|context| async move { Ok(context.code_version()) }.boxed_local()))
        .unwrap();
    assert_eq!(epoch, 2);
    let clock = tembra_core::Driver::now(driver.as_ref());
    send(&raw, &controls()[5]);
    third["transaction"] = json!(3);
    send(&raw, &third);
    let vote = next(&driver, &mut raw);
    assert_eq!(vote["outcome"]["status"], "ready");
    assert_eq!(vote["code_version"], 3);
    assert_eq!(tembra_core::Driver::now(driver.as_ref()), clock);
    send(
        &raw,
        &json!({"t":"reload_commit","transaction":3,"code_version":3}),
    );
    assert_eq!(next(&driver, &mut raw)["t"], "reload_ack");
    let epoch = driver
        .run_until(host.root(|context| async move { Ok(context.code_version()) }.boxed_local()))
        .unwrap();
    assert_eq!(epoch, 3);
    driver.run_until(host.close()).unwrap();
}
