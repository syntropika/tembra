use super::*;
use crate::{
    GetOptions, Host, HostOptions, Implementation, ManualDriver, ReplaceOptions, StateDeclaration,
};
use futures::StreamExt;
use serde_json::json;
use std::collections::BTreeMap;
use tembra_contracts::{Contract, Member, Placement};

fn receive(driver: &ManualDriver, link: &mut Link) -> Value {
    loop {
        if let Some(LinkEvent::Message(bytes)) = driver.run_until(link.events.next()) {
            return tembra_contracts::parse_json(std::str::from_utf8(&bytes).unwrap()).unwrap();
        }
    }
}
fn raw(link: &Link, value: Value) {
    link.tx
        .try_send(Rc::from(serde_json::to_vec(&value).unwrap()))
        .unwrap();
}

#[test]
fn initial_unit_reserves_all_credit_before_flush_can_send_another_dirty_hold() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("server", json!("host")), driver.clone());
    host.set_rpc_identity(RootOrigin::new("server", "00000000000000000000000000000001").unwrap())
        .unwrap();
    let contract = Rc::new(
        Contract::remote(
            "atomic-unit",
            1,
            json!({}),
            Placement::Named {
                name: "server".into(),
            },
            BTreeMap::from([(
                "v".into(),
                BTreeMap::from([(
                    "state".into(),
                    Member::State {
                        schema: json!({"type":"string"}),
                    },
                )]),
            )]),
        )
        .unwrap(),
    );
    let saved = Rc::new(RefCell::new(None));
    let capture = saved.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let cell = declaration.state(
                "state",
                StateDeclaration::new(json!({"type":"string"}), json!("")),
            )?;
            *capture.borrow_mut() = Some(cell.clone());
            Ok(Implementation::new()
                .variant("v")
                .state("state", cell.readable()))
        }),
        Default::default(),
    )
    .unwrap();
    let (mut link, provider, _) =
        super::super::super::memory_link(driver.clone(), Default::default()).unwrap();
    let mut options = ConnectionOptions::default();
    options.limits.max_frame_bytes = 4096;
    options.limits.queued_bytes = 65536;
    options.limits.state_in_flight = 4096;
    options.limits.state_window = 4096;
    let connection = host
        .accept_client(provider, json!("user"), options)
        .unwrap();
    raw(
        &link,
        json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,"peer":{"role":"client"},"receive":{"max_message_bytes":4096,"max_unit_bytes":65536,"state_window_bytes":4096}}}),
    );
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(receive(&driver, &mut link)["t"], "hello");
    raw(
        &link,
        json!({"t":"hold","hold":1,"target":"server","consumer":contract.descriptor(),"input":{},"state":true}),
    );
    assert_eq!(receive(&driver, &mut link)["t"], "shape");
    let initial = receive(&driver, &mut link);
    raw(
        &link,
        json!({"t":"state_ack","key":initial["key"],"revision":0}),
    );
    driver.run_until_stalled();
    let cell = saved.borrow().as_ref().unwrap().clone();
    for text in ["x", "y"] {
        let cell = cell.clone();
        driver
            .run_until(host.root(move |ctx| {
                async move {
                    cell.replace(&ctx, json!(text.repeat(2800)), ReplaceOptions::default())?;
                    Ok(())
                }
                .boxed_local()
            }))
            .unwrap();
        if text == "x" {
            assert_eq!(receive(&driver, &mut link)["t"], "delta");
        }
    }
    // Reproduce the valid post-ACK, pre-flush scheduling point. Capture of a new unit
    // must not let the old dirty feed consume the credit that it just preflighted.
    {
        let mut streams = connection.inner.streams.borrow_mut();
        let feed = streams.values_mut().next().unwrap();
        let frontier = feed.outstanding.take().unwrap();
        connection
            .inner
            .state_bytes
            .set(connection.inner.state_bytes.get() - frontier.bytes);
        feed.acked = Some(frontier.snapshot);
        assert!(feed.dirty);
    }
    let first = connection
        .inner
        .served
        .borrow()
        .get(&HoldId::new(1).unwrap())
        .unwrap()
        .clone();
    let reference = host
        .hold(
            contract.clone(),
            json!({}),
            GetOptions {
                principal: Some(json!("user")),
                ..Default::default()
            },
        )
        .unwrap();
    let record = Rc::new(ServedHold {
        reference,
        request: first.request.clone(),
        principal: json!("user"),
        unit: Cell::new(0),
        subscriptions: RefCell::default(),
        state_on: Cell::new(true),
        ready: Cell::new(false),
        generation: Cell::new(1),
        status_subscription: RefCell::default(),
    });
    let hold = HoldId::new(2).unwrap();
    connection.inner.received_hold.set(2);
    connection
        .inner
        .served
        .borrow_mut()
        .insert(hold, record.clone());
    connection
        .capture_unit(
            hold,
            &record,
            "v".into(),
            vec![("state".into(), cell.readable())],
        )
        .unwrap();
    assert!(
        connection.inner.state_bytes.get() <= connection.limits().state_in_flight,
        "nested flushing exceeded the checked global state-credit budget"
    );
    let shape = receive(&driver, &mut link);
    let snapshot = receive(&driver, &mut link);
    assert_eq!(shape["t"], "shape");
    assert_eq!(shape["hold"], 2);
    assert_eq!(snapshot["t"], "snapshot");
    assert_eq!(
        snapshot["key"]["hold"], 2,
        "no other hold delta may split the captured initial unit"
    );
    driver.run_until(host.close()).unwrap();
}

struct GateTx {
    blocked: Rc<Cell<bool>>,
    inner: Rc<dyn LinkTx>,
}
impl LinkTx for GateTx {
    fn try_send(&self, bytes: Rc<[u8]>) -> std::result::Result<(), SendError> {
        if self.blocked.get() {
            Err(SendError::WouldBlock)
        } else {
            self.inner.try_send(bytes)
        }
    }
    fn close(&self, reason: CloseReason) {
        self.inner.close(reason);
    }
}

#[test]
fn state_admission_leaves_room_for_a_full_method_result_and_ping_reply() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("server", json!("host")), driver.clone());
    host.set_rpc_identity(RootOrigin::new("server", "00000000000000000000000000000001").unwrap())
        .unwrap();
    let contract = Rc::new(
        Contract::remote(
            "reserved-control",
            1,
            json!({}),
            Placement::Named {
                name: "server".into(),
            },
            BTreeMap::from([(
                "v".into(),
                BTreeMap::from([
                    (
                        "state".into(),
                        Member::State {
                            schema: json!({"type":"string"}),
                        },
                    ),
                    (
                        "result".into(),
                        Member::Method {
                            args: None,
                            result: Some(tembra_contracts::ValueSchema::Portable(
                                json!({"type":"string"}),
                            )),
                        },
                    ),
                ]),
            )]),
        )
        .unwrap(),
    );
    host.serve(
        contract.clone(),
        Rc::new(|_, _, declaration| {
            let cell = declaration.state(
                "state",
                StateDeclaration::new(json!({"type":"string"}), json!("s".repeat(2800))),
            )?;
            Ok(Implementation::new()
                .variant("v")
                .state("state", cell.readable())
                .json_method("result", |_, _| {
                    async { Ok(Some(json!("r".repeat(3900)))) }.boxed_local()
                }))
        }),
        Default::default(),
    )
    .unwrap();
    let (mut link, provider, _) =
        super::super::super::memory_link(driver.clone(), Default::default()).unwrap();
    let blocked = Rc::new(Cell::new(false));
    let provider = Link {
        tx: Rc::new(GateTx {
            blocked: blocked.clone(),
            inner: provider.tx,
        }),
        events: provider.events,
    };
    let mut options = ConnectionOptions::default();
    options.limits.max_frame_bytes = 4096;
    options.limits.queued_bytes = 65536;
    options.limits.state_in_flight = 65536;
    options.limits.state_window = 4096;
    let connection = host
        .accept_client(provider, json!("user"), options)
        .unwrap();
    raw(
        &link,
        json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,"peer":{"role":"client"},"receive":{"max_message_bytes":4096,"max_unit_bytes":65536,"state_window_bytes":4096}}}),
    );
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(receive(&driver, &mut link)["t"], "hello");
    blocked.set(true);
    let mut refused = None;
    for hold in 1..=128 {
        raw(
            &link,
            json!({"t":"hold","hold":hold,"target":"server","consumer":contract.descriptor(),"input":{},"state":true}),
        );
        driver.run_until_stalled();
        if !connection
            .inner
            .served
            .borrow()
            .contains_key(&HoldId::new(hold).unwrap())
        {
            refused = Some(hold);
            break;
        }
    }
    assert!(
        refused.is_some(),
        "the bounded data sub-budget must eventually refuse a unit"
    );
    raw(
        &link,
        json!({"t":"call","call":1,"hold":1,"member":"result","args":{"presence":"absent"}}),
    );
    raw(&link, json!({"t":"ping","nonce":7}));
    driver.run_until_stalled();
    assert_eq!(
        connection.status(),
        ConnectionStatus::Open,
        "state cannot consume reserved control admission"
    );
    blocked.set(false);
    connection.flush();
    let (mut result, mut pong, mut rejected) = (false, false, false);
    while !(result && pong && rejected) {
        let frame = receive(&driver, &mut link);
        match frame["t"].as_str().unwrap() {
            "result" => {
                assert_eq!(frame["outcome"]["status"], "success");
                assert_eq!(
                    frame["outcome"]["data"]["value"].as_str().unwrap().len(),
                    3900
                );
                result = true;
            }
            "pong" => {
                assert_eq!(frame["nonce"], 7);
                pong = true;
            }
            "gone" => {
                assert_eq!(frame["hold"], refused.unwrap());
                assert_eq!(frame["error"]["reason"], "unit_limit");
                rejected = true;
            }
            "shape" | "snapshot" => {}
            other => panic!("unexpected frame {other}"),
        }
    }
    driver.run_until(host.close()).unwrap();
}
