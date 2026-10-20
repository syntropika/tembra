use std::{cell::Cell, collections::BTreeMap, rc::Rc};

use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use tembra_core::{
    Host, HostOptions, Implementation, ManualDriver,
    contracts::{Contract, Member, Placement, ValueSchema},
    rpc::{
        Connection, ConnectionOptions, Link, LinkEvent, MemoryLinkOptions, RootOrigin, memory_link,
    },
};

struct Fixture {
    driver: Rc<ManualDriver>,
    host: Host,
    raw: Link,
    connection: Connection,
    effects: Rc<Cell<usize>>,
}
fn send(link: &Link, value: Value) {
    link.tx
        .try_send(Rc::from(serde_json::to_vec(&value).unwrap()))
        .unwrap();
}
fn next(driver: &ManualDriver, link: &mut Link) -> Value {
    loop {
        match driver.run_until(link.events.next()) {
            Some(LinkEvent::Message(bytes)) => {
                return tembra_core::contracts::parse_json(std::str::from_utf8(&bytes).unwrap())
                    .unwrap();
            }
            Some(LinkEvent::Opened | LinkEvent::Writable) => {}
            event => panic!("expected frame, got {event:?}"),
        }
    }
}
fn root(sequence: u64) -> Value {
    json!({"origin":{"host":"origin","incarnation":"00000000000000000000000000000003"},"sequence":sequence})
}
fn fixture(history: usize) -> Fixture {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("server", json!("server-host")),
        driver.clone(),
    );
    host.set_rpc_identity(RootOrigin::new("server", "00000000000000000000000000000001").unwrap())
        .unwrap();
    let contract = Rc::new(
        Contract::remote(
            "rpc.roots",
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
                        result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                    },
                )]),
            )]),
        )
        .unwrap(),
    );
    let effects = Rc::new(Cell::new(0));
    let captured = effects.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, _| {
            let effects = captured.clone();
            Ok(Implementation::new()
                .variant("v")
                .json_method("run", move |_, _| {
                    effects.set(effects.get() + 1);
                    let value = effects.get();
                    async move { Ok(Some(json!(value))) }.boxed_local()
                }))
        }),
        Default::default(),
    )
    .unwrap();
    let (mut raw, provider, _) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let mut options = ConnectionOptions::default();
    options.limits.max_ended_roots_per_connection = history;
    let connection = host
        .accept_child(provider, "peer", json!("accepted-peer"), options)
        .unwrap();
    send(
        &raw,
        json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,"peer":{"role":"host","identity":{"host":"peer","incarnation":"00000000000000000000000000000002"},"code_version":1},"receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,"state_window_bytes":262144}}}),
    );
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(&driver, &mut raw)["t"], "hello");
    send(
        &raw,
        json!({"t":"hold","hold":1,"target":"server","consumer":contract.descriptor(),"input":{},"state":false}),
    );
    assert_eq!(next(&driver, &mut raw)["t"], "shape");
    Fixture {
        driver,
        host,
        raw,
        connection,
        effects,
    }
}
fn call(fixture: &mut Fixture, call: u64, identity: Value, version: u64) -> Value {
    send(
        &fixture.raw,
        json!({"t":"trusted_call","call":call,"hold":1,"member":"run","args":{"presence":"absent"},"root":{"identity":identity,"code_version":version}}),
    );
    next(&fixture.driver, &mut fixture.raw)
}

#[test]
fn ended_carrier_is_exact_not_high_water_and_unknown_end_cannot_preempt_roots() {
    let mut fixture = fixture(4096);
    send(&fixture.raw, json!({"t":"root_end","root":root(2)}));
    let first = call(&mut fixture, 1, root(2), 1);
    assert_eq!(first["outcome"]["status"], "success");
    send(&fixture.raw, json!({"t":"root_end","root":root(2)}));
    let ended = call(&mut fixture, 2, root(2), 1);
    assert_eq!(ended["outcome"]["data"]["code"], "aborted");
    assert_eq!(ended["outcome"]["data"]["reason"], "root_ended");
    let older = call(&mut fixture, 3, root(1), 1);
    assert_eq!(older["outcome"]["status"], "success");
    let epoch = call(&mut fixture, 4, root(1), 2);
    assert_eq!(epoch["outcome"]["data"]["code"], "superseded");
    assert_eq!(epoch["outcome"]["data"]["reason"], "code_version");
    let unissued = json!({"origin":{"host":"server","incarnation":"00000000000000000000000000000001"},"sequence":99999});
    assert_eq!(
        call(&mut fixture, 5, unissued, 1)["outcome"]["status"],
        "error"
    );
    assert_eq!(fixture.effects.get(), 2);
    fixture.driver.run_until(fixture.host.close()).unwrap();
}

#[test]
fn exact_foreign_history_closes_before_forgetting_an_ended_root() {
    let mut fixture = fixture(1);
    assert_eq!(
        call(&mut fixture, 1, root(1), 1)["outcome"]["status"],
        "success"
    );
    send(&fixture.raw, json!({"t":"root_end","root":root(1)}));
    assert_eq!(
        call(&mut fixture, 2, root(2), 1)["outcome"]["status"],
        "success"
    );
    send(&fixture.raw, json!({"t":"root_end","root":root(2)}));
    assert_eq!(
        fixture
            .driver
            .run_until(fixture.connection.closed())
            .error
            .reason,
        "root_history"
    );
    assert_eq!(fixture.effects.get(), 2);
    fixture.driver.run_until(fixture.host.close()).unwrap();
}
