//! Adversarial public Connection acceptance added after the M4 static review.

use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{
    ErrorCode, Host, HostOptions, Implementation, ManualDriver, Readable, RefStatus, Reference,
    StateEvent, Subscription,
    contracts::{Contract, Member, Placement},
    rpc::{
        CloseReason, Connection, ConnectionOptions, ConnectionStatus, Link, LinkEvent, RootOrigin,
        memory_link,
    },
};

fn host(driver: &Rc<ManualDriver>, name: &str) -> Host {
    let host = Host::new(
        HostOptions::new(name, json!("host-principal")),
        driver.clone(),
    );
    host.set_rpc_identity(RootOrigin::new(name, "11111111111111111111111111111111").unwrap())
        .unwrap();
    host
}

fn send(link: &Link, frame: Value) {
    link.tx
        .try_send(Rc::from(serde_json::to_vec(&frame).unwrap()))
        .unwrap();
}

fn next(driver: &ManualDriver, link: &mut Link) -> Value {
    driver.run_until_stalled();
    loop {
        let event = link
            .events
            .next()
            .now_or_never()
            .expect("expected an immediately available frame after draining local jobs");
        match event {
            Some(LinkEvent::Message(bytes)) => {
                return tembra_core::contracts::parse_json(std::str::from_utf8(&bytes).unwrap())
                    .unwrap();
            }
            Some(LinkEvent::Opened | LinkEvent::Writable) => {}
            other => panic!("expected a protocol frame, received {other:?}"),
        }
    }
}

fn hello(peer: Value) -> Value {
    json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,"peer":peer,"receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,"state_window_bytes":262144}}})
}

fn host_peer(name: &str) -> Value {
    json!({"role":"host","identity":{"host":name,"incarnation":"22222222222222222222222222222222"},"code_version":1})
}

fn remote(name: &str, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(
        Contract::remote(
            name,
            1,
            json!({}),
            Placement::Named {
                name: "server".into(),
            },
            BTreeMap::from([("v".into(), members)]),
        )
        .unwrap(),
    )
}

fn root_reference() -> Value {
    json!({"identity":{"origin":{"host":"peer","incarnation":"22222222222222222222222222222222"},"sequence":1},"code_version":1})
}

#[test]
fn client_and_child_control_authority_is_refused_at_the_configured_role_boundary() {
    for (child, kind) in [
        (false, "trusted_call"),
        (false, "forward_hold"),
        (false, "invalidate_principal"),
        (true, "forward_hold"),
        (true, "invalidate_principal"),
    ] {
        let driver = ManualDriver::new();
        let server = host(&driver, "server");
        let contract = remote(
            "rpc.review.roles",
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            )]),
        );
        let entered = Rc::new(Cell::new(0));
        let factories = Rc::new(Cell::new(0));
        let (observed, created) = (entered.clone(), factories.clone());
        server
            .serve(
                contract.clone(),
                Rc::new(move |_, _, _| {
                    created.set(created.get() + 1);
                    let entered = observed.clone();
                    Ok(Implementation::new()
                        .variant("v")
                        .json_method("run", move |_, _| {
                            entered.set(entered.get() + 1);
                            async { Ok(None) }.boxed_local()
                        }))
                }),
                Default::default(),
            )
            .unwrap();
        let (mut raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
        let connection = if child {
            server
                .accept_child(
                    incoming,
                    "peer",
                    json!("authenticated"),
                    ConnectionOptions::default(),
                )
                .unwrap()
        } else {
            server
                .accept_client(
                    incoming,
                    json!("authenticated"),
                    ConnectionOptions::default(),
                )
                .unwrap()
        };
        send(
            &raw,
            hello(if child {
                host_peer("peer")
            } else {
                json!({"role":"client"})
            }),
        );
        driver.run_until(connection.ready()).unwrap();
        assert_eq!(next(&driver, &mut raw)["t"], "hello");
        send(
            &raw,
            json!({"t":"hold","hold":1,"target":"server","consumer":contract.descriptor(),"input":{},"state":false}),
        );
        assert_eq!(next(&driver, &mut raw)["t"], "shape");
        assert_eq!(factories.get(), 1);
        let malicious = match kind {
            "trusted_call" => {
                json!({"t":kind,"call":1,"hold":1,"member":"run","args":{"presence":"absent"},"root":root_reference()})
            }
            "forward_hold" => {
                json!({"t":kind,"hold":2,"target":"server","consumer":contract.descriptor(),"input":{},"state":false,"caller":{"principal":"forged-admin","origin":"forged","authorization_generation":0}})
            }
            "invalidate_principal" => {
                json!({"t":kind,"principal":"authenticated","origin":"forged","through_generation":0})
            }
            _ => unreachable!(),
        };
        send(&raw, malicious);
        driver.run_until_stalled();
        let reason = connection
            .closed()
            .now_or_never()
            .expect("forbidden frame must immediately close, not time out");
        assert_eq!(
            reason.error.code,
            ErrorCode::Forbidden,
            "child={child}, frame={kind}"
        );
        assert_eq!(
            reason.error.reason, "rpc_role",
            "child={child}, frame={kind}"
        );
        assert_eq!(connection.status(), ConnectionStatus::Closed);
        assert_eq!(entered.get(), 0);
        assert_eq!(
            factories.get(),
            1,
            "forged principal must not create an object"
        );
        driver.run_until(server.close()).unwrap();
    }
}

#[test]
fn local_contract_and_inconsistent_wire_placement_are_per_hold_typed_refusals() {
    let driver = ManualDriver::new();
    let server = host(&driver, "server");
    let contract = remote("rpc.review.placement", BTreeMap::new());
    let created = Rc::new(Cell::new(0));
    let observed = created.clone();
    server
        .serve(
            contract.clone(),
            Rc::new(move |_, _, _| {
                observed.set(observed.get() + 1);
                Ok(Implementation::new().variant("v"))
            }),
            Default::default(),
        )
        .unwrap();
    let (mut raw, incoming, _) = memory_link(driver.clone(), Default::default()).unwrap();
    let connection = server
        .accept_client(incoming, json!("user"), ConnectionOptions::default())
        .unwrap();
    send(&raw, hello(json!({"role":"client"})));
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(&driver, &mut raw)["t"], "hello");
    let local = Contract::local("rpc.review.local", 1, json!({}), BTreeMap::new()).unwrap();
    send(
        &raw,
        json!({"t":"hold","hold":1,"target":"server","consumer":local.descriptor(),"input":{},"state":false}),
    );
    let refused = next(&driver, &mut raw);
    assert_eq!(refused["t"], "gone");
    assert_eq!(refused["hold"], 1);
    assert_eq!(refused["error"]["code"], "invalid");
    assert_eq!(refused["error"]["reason"], "remote_contract");
    send(
        &raw,
        json!({"t":"hold","hold":2,"target":"another-target","consumer":contract.descriptor(),"input":{},"state":false}),
    );
    let refused = next(&driver, &mut raw);
    assert_eq!(refused["t"], "gone");
    assert_eq!(refused["hold"], 2);
    assert_eq!(refused["error"]["code"], "invalid");
    assert_eq!(refused["error"]["reason"], "placement");
    assert_eq!(created.get(), 0);
    assert_eq!(connection.status(), ConnectionStatus::Open);
    send(
        &raw,
        json!({"t":"hold","hold":3,"target":"server","consumer":contract.descriptor(),"input":{},"state":false}),
    );
    assert_eq!(next(&driver, &mut raw)["t"], "shape");
    assert_eq!(
        created.get(),
        1,
        "refused holds must not poison a later valid acquisition"
    );
    driver.run_until(server.close()).unwrap();
}

struct Replica {
    driver: Rc<ManualDriver>,
    host: Host,
    connection: Connection,
    raw: Link,
    reference: Reference,
    readable: Readable,
    key: Value,
    events: Rc<RefCell<Vec<StateEvent>>>,
    _subscription: Subscription,
}

fn replica() -> Replica {
    let driver = ManualDriver::new();
    let host = host(&driver, "client");
    let contract = remote(
        "rpc.review.replica",
        BTreeMap::from([(
            "state".into(),
            Member::State {
                schema: json!({"type":"object", "additionalProperties":true}),
            },
        )]),
    );
    let (outgoing, mut raw, _) = memory_link(driver.clone(), Default::default()).unwrap();
    let mut options = ConnectionOptions::default();
    options.limits.max_delta_ops = 1;
    options.limits.max_path_segments = 1;
    let connection = host.connect_client(outgoing, "server", options).unwrap();
    send(&raw, hello(host_peer("server")));
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(&driver, &mut raw)["t"], "hello");
    host.set_route("server", connection.clone()).unwrap();
    let reference = host
        .hold(contract.clone(), json!({}), Default::default())
        .unwrap();
    let hold = next(&driver, &mut raw);
    assert_eq!(hold["t"], "hold");
    send(
        &raw,
        json!({"t":"shape","hold":hold["hold"],"unit":1,"consumer_fingerprint":contract.fingerprint(),"provider":contract.descriptor(),"mode":{"mode":"direct"},"variant":"v","members":["state"],"state":[{"member":"state","stream":1}]}),
    );
    let key = json!({"hold":hold["hold"],"unit":1,"member":"state","stream":1});
    send(
        &raw,
        json!({"t":"snapshot","key":key,"snapshot":{"value":{"items":[1],"text":"😀","drop":null},"revision":0}}),
    );
    let ack = next(&driver, &mut raw);
    assert_eq!(ack["t"], "state_ack");
    assert_eq!(ack["revision"], 0);
    driver.run_until(reference.ready()).unwrap();
    let readable = reference.readable("state").unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let received = events.clone();
    let subscription = readable
        .subscribe_events(move |event| {
            received.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    Replica {
        driver,
        host,
        connection,
        raw,
        reference,
        readable,
        key,
        events,
        _subscription: subscription,
    }
}

fn recover_without_bad_publication(mut replica: Replica, ops: Value) {
    let before = replica.readable.snapshot().unwrap();
    send(
        &replica.raw,
        json!({"t":"delta","key":replica.key,"batch":{"base_revision":0,"result_revision":1,"ops":ops}}),
    );
    let resync = next(&replica.driver, &mut replica.raw);
    assert_eq!(resync["t"], "resync");
    assert_eq!(resync["key"], replica.key);
    assert_eq!(resync["failed_revision"], 1);
    assert_eq!(resync["seen_revision"], 0);
    assert_eq!(replica.connection.status(), ConnectionStatus::Open);
    assert_eq!(replica.reference.status(), RefStatus::Live);
    let unchanged = replica.readable.snapshot().unwrap();
    assert_eq!(unchanged.revision(), before.revision());
    assert_eq!(unchanged.value(), before.value());
    assert_eq!(
        replica.events.borrow().len(),
        1,
        "rejected operation must never publish an Ops event"
    );
    let mut rotated = replica.key.clone();
    rotated["stream"] = json!(2);
    send(
        &replica.raw,
        json!({"t":"reset","key":rotated,"snapshot":{"value":{"items":[2],"text":"😃"},"revision":1}}),
    );
    let ack = next(&replica.driver, &mut replica.raw);
    assert_eq!(ack["t"], "state_ack");
    assert_eq!(ack["key"], rotated);
    assert_eq!(ack["revision"], 1);
    assert_eq!(
        replica.readable.snapshot().unwrap().value(),
        &json!({"items":[2],"text":"😃"})
    );
    assert_eq!(replica.events.borrow().len(), 2);
    assert!(matches!(
        replica.events.borrow().last(),
        Some(StateEvent::Reset(_))
    ));
    assert_eq!(
        before.value(),
        &json!({"items":[1],"text":"😀","drop":null})
    );
    replica.connection.close(CloseReason::closed());
    replica.driver.run_until(replica.host.close()).unwrap();
}

#[test]
fn malformed_operation_decodes_to_exact_resync_without_adopting_bad_state() {
    recover_without_bad_publication(replica(), json!([{"op":"not-a-delta-operation","path":[]}]));
}

#[test]
fn checked_delta_over_local_operation_budget_recovers_without_partial_replay() {
    recover_without_bad_publication(
        replica(),
        json!([
            {"op":"set","path":[],"value":{"items":[9]}},
            {"op":"set","path":[],"value":{"items":[10]}}
        ]),
    );
}

#[test]
fn checked_delta_over_local_path_budget_recovers_without_partial_replay() {
    recover_without_bad_publication(
        replica(),
        json!([{
            "op":"set","path":[{"kind":"key","value":"items"},{"kind":"index","value":0}],"value":9
        }]),
    );
}
