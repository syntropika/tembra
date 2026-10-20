use std::{cell::Cell, collections::BTreeMap, rc::Rc};

use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use tembra_core::{
    GetOptions, Host, HostOptions, Implementation, ManualDriver,
    contracts::{Contract, Member, Placement},
    rpc::{ConnectionOptions, Link, LinkEvent, MemoryLinkOptions, RootOrigin, memory_link},
};

fn send(link: &Link, frame: Value) {
    link.tx
        .try_send(Rc::from(serde_json::to_vec(&frame).unwrap()))
        .unwrap();
}
fn next(driver: &ManualDriver, link: &mut Link) -> Value {
    loop {
        match driver.run_until(link.events.next()) {
            Some(LinkEvent::Message(bytes)) => return serde_json::from_slice(&bytes).unwrap(),
            Some(LinkEvent::Opened | LinkEvent::Writable) => {}
            event => panic!("expected frame, got {event:?}"),
        }
    }
}

#[test]
fn parent_revocation_is_exact_and_scoped_by_origin_generation_and_connection() {
    let driver = ManualDriver::new();
    let server = Host::new(HostOptions::new("server", json!("host")), driver.clone());
    server
        .set_rpc_identity(RootOrigin::new("server", "00000000000000000000000000000001").unwrap())
        .unwrap();
    let consumer = Rc::new(
        Contract::remote(
            "rpc.authority",
            1,
            json!({}),
            Placement::Named {
                name: "server".into(),
            },
            BTreeMap::from([(
                "v".into(),
                BTreeMap::from([(
                    "run".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                )]),
            )]),
        )
        .unwrap(),
    );
    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    server
        .serve(
            consumer.clone(),
            Rc::new(move |_, _, _| {
                let calls = captured.clone();
                Ok(Implementation::new()
                    .variant("v")
                    .json_method("run", move |_, _| {
                        calls.set(calls.get() + 1);
                        async { Ok(None) }.boxed_local()
                    }))
            }),
            Default::default(),
        )
        .unwrap();
    let principal = json!({"id":"user","scope":"full"});
    let (mut raw, provider, _) = memory_link(driver.clone(), Default::default()).unwrap();
    let connection = server
        .connect_parent(
            provider,
            "parent",
            json!("parent-ingress"),
            ConnectionOptions::default(),
        )
        .unwrap();
    send(
        &raw,
        json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,"peer":{"role":"host","identity":{"host":"parent","incarnation":"00000000000000000000000000000002"},"code_version":1},"receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,"state_window_bytes":262144}}}),
    );
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(&driver, &mut raw)["t"], "hello");
    for (hold, origin, generation) in [(1, "first", 5), (2, "first", 6), (3, "second", 5)] {
        send(
            &raw,
            json!({"t":"forward_hold","hold":hold,"target":"server","consumer":consumer.descriptor(),"input":{},"state":false,"caller":{"principal":principal,"origin":origin,"authorization_generation":generation}}),
        );
        assert_eq!(next(&driver, &mut raw)["t"], "shape");
    }
    let client = Host::new(HostOptions::new("client", Value::Null), driver.clone());
    let (client_link, server_link, _) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let _sibling = server
        .accept_client(server_link, principal.clone(), ConnectionOptions::default())
        .unwrap();
    let outgoing = client
        .connect_client(client_link, "server", ConnectionOptions::default())
        .unwrap();
    client.set_route("server", outgoing).unwrap();
    let sibling = driver
        .run_until(client.get(consumer.clone(), json!({}), GetOptions::default()))
        .unwrap();

    send(
        &raw,
        json!({"t":"invalidate_principal","principal":{"id":"user"},"origin":"first","through_generation":5}),
    );
    send(
        &raw,
        json!({"t":"call","call":1,"hold":1,"member":"run","args":{"presence":"absent"}}),
    );
    assert_eq!(next(&driver, &mut raw)["outcome"]["status"], "success");
    send(
        &raw,
        json!({"t":"invalidate_principal","principal":principal,"origin":"first","through_generation":5}),
    );
    let gone = next(&driver, &mut raw);
    assert_eq!(gone["t"], "gone");
    assert_eq!(gone["hold"], 1);
    for (call, hold) in [(2, 2), (3, 3)] {
        send(
            &raw,
            json!({"t":"call","call":call,"hold":hold,"member":"run","args":{"presence":"absent"}}),
        );
        assert_eq!(next(&driver, &mut raw)["outcome"]["status"], "success");
    }
    driver
        .run_until(sibling.call_json("run", None, None))
        .unwrap();
    assert_eq!(calls.get(), 4);
    send(
        &raw,
        json!({"t":"forward_hold","hold":4,"target":"server","consumer":consumer.descriptor(),"input":{},"state":false,"caller":{"principal":principal,"origin":"first","authorization_generation":4}}),
    );
    let gone = next(&driver, &mut raw);
    assert_eq!(gone["t"], "gone");
    assert_eq!(gone["hold"], 4);
    assert_eq!(gone["error"]["reason"], "revoked");
    driver.run_until(client.close()).unwrap();
    driver.run_until(server.close()).unwrap();
}

fn parent(
    driver: &Rc<ManualDriver>,
    host: &Host,
    options: ConnectionOptions,
) -> (Link, tembra_core::rpc::Connection) {
    let (mut raw, provider, _) = memory_link(driver.clone(), Default::default()).unwrap();
    let connection = host
        .connect_parent(provider, "parent", Value::Null, options)
        .unwrap();
    send(
        &raw,
        json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,"peer":{"role":"host","identity":{"host":"parent","incarnation":"00000000000000000000000000000002"},"code_version":1},"receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,"state_window_bytes":262144}}}),
    );
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(driver, &mut raw)["t"], "hello");
    (raw, connection)
}
fn invalidate(raw: &Link, principal: Value, origin: &str, generation: u64) {
    send(
        raw,
        json!({"t":"invalidate_principal","principal":principal,"origin":origin,"through_generation":generation}),
    );
}

#[test]
fn revocation_history_has_exact_count_and_utf8_byte_limits_without_free_updates() {
    for utf8_limit in [false, true] {
        let driver = ManualDriver::new();
        let server = Host::new(HostOptions::new("server", Value::Null), driver.clone());
        server
            .set_rpc_identity(
                RootOrigin::new("server", "00000000000000000000000000000001").unwrap(),
            )
            .unwrap();
        let mut options = ConnectionOptions::default();
        options.limits.max_revocation_history = 1;
        options.limits.max_revocation_bytes = if utf8_limit { 4 } else { 1024 };
        let (raw, connection) = parent(&driver, &server, options);
        if utf8_limit {
            // canonical principal is four UTF-8 bytes including quotes, plus one origin byte.
            invalidate(&raw, json!("é"), "x", 0);
        } else {
            invalidate(&raw, Value::Null, "x", 0);
            invalidate(&raw, Value::Null, "x", 99);
            driver.run_until_stalled();
            assert_eq!(
                connection.status(),
                tembra_core::rpc::ConnectionStatus::Open
            );
            invalidate(&raw, Value::Null, "y", 0);
        }
        assert_eq!(
            driver.run_until(connection.closed()).error.reason,
            "revocation_history"
        );
        driver.run_until(server.close()).unwrap();
    }
}

#[test]
fn host_revocation_budget_is_shared_and_closing_one_carrier_releases_only_its_history() {
    let driver = ManualDriver::new();
    let limits = tembra_core::HostRpcLimits {
        max_revocation_history: 1,
        ..Default::default()
    };
    let server = Host::new(
        HostOptions::new("server", Value::Null)
            .with_rpc_limits(limits)
            .unwrap(),
        driver.clone(),
    );
    server
        .set_rpc_identity(RootOrigin::new("server", "00000000000000000000000000000001").unwrap())
        .unwrap();
    let (first, live) = parent(&driver, &server, ConnectionOptions::default());
    invalidate(&first, Value::Null, "x", 0);
    driver.run_until_stalled();
    let (second, refused) = parent(&driver, &server, ConnectionOptions::default());
    invalidate(&second, Value::Null, "x", 0);
    assert_eq!(
        driver.run_until(refused.closed()).error.reason,
        "revocation_limit"
    );
    assert_eq!(live.status(), tembra_core::rpc::ConnectionStatus::Open);
    live.close(tembra_core::rpc::CloseReason::closed());
    let (third, accepted) = parent(&driver, &server, ConnectionOptions::default());
    invalidate(&third, Value::Null, "x", 0);
    driver.run_until_stalled();
    assert_eq!(accepted.status(), tembra_core::rpc::ConnectionStatus::Open);
    driver.run_until(server.close()).unwrap();
}
