use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use tembra_core::{
    Cell, Host, HostOptions, Implementation, ManualDriver, ReplaceOptions, StateDeclaration,
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
    cell: Cell,
    other: Cell,
    outcomes: Rc<std::cell::Cell<usize>>,
    initial: Value,
    contract: Rc<Contract>,
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
            event => panic!("expected message, got {event:?}"),
        }
    }
}
fn ack(link: &Link, event: &Value) {
    send(
        link,
        json!({"t":"state_ack","key":event["key"],"revision":event.get("snapshot").map_or_else(|| event["batch"]["result_revision"].clone(), |snapshot| snapshot["revision"].clone())}),
    );
}
fn fixture() -> Fixture {
    fixture_schema(json!({"type":"integer"}))
}
fn fixture_schema(schema: Value) -> Fixture {
    let fixture = fixture_options(schema, ConnectionOptions::default());
    assert_eq!(fixture.initial["t"], "shape");
    fixture
}
fn fixture_options(schema: Value, options: ConnectionOptions) -> Fixture {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("server", json!("host")), driver.clone());
    host.set_rpc_identity(RootOrigin::new("server", "00000000000000000000000000000001").unwrap())
        .unwrap();
    let contract = Rc::new(
        Contract::remote(
            "rpc.flow",
            1,
            json!({}),
            Placement::Named {
                name: "server".into(),
            },
            BTreeMap::from([(
                "v".into(),
                BTreeMap::from([
                    (
                        "a".into(),
                        Member::State {
                            schema: schema.clone(),
                        },
                    ),
                    (
                        "b".into(),
                        Member::State {
                            schema: schema.clone(),
                        },
                    ),
                    (
                        "large".into(),
                        Member::Method {
                            args: None,
                            result: Some(ValueSchema::Portable(json!({}))),
                        },
                    ),
                    (
                        "deep".into(),
                        Member::Method {
                            args: None,
                            result: Some(ValueSchema::Portable(json!({}))),
                        },
                    ),
                    (
                        "ping".into(),
                        Member::Method {
                            args: None,
                            result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                        },
                    ),
                ]),
            )]),
        )
        .unwrap(),
    );
    let cells = Rc::new(RefCell::new(None));
    let captured = cells.clone();
    let outcomes = Rc::new(std::cell::Cell::new(0));
    let observed = outcomes.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let a = declaration.state("a", StateDeclaration::new(schema.clone(), json!(0)))?;
            let b = declaration.state("b", StateDeclaration::new(schema.clone(), json!(10)))?;
            *captured.borrow_mut() = Some((a.clone(), b.clone()));
            let large_outcomes = observed.clone();
            let deep_outcomes = observed.clone();
            Ok(Implementation::new()
                .variant("v")
                .state("a", a.readable())
                .state("b", b.readable())
                .json_method("ping", |_, _| async { Ok(Some(json!(7))) }.boxed_local())
                .json_method("large", move |_, _| {
                    large_outcomes.set(large_outcomes.get() + 1);
                    async { Ok(Some(wire_ineligible(false))) }.boxed_local()
                })
                .json_method("deep", move |_, _| {
                    deep_outcomes.set(deep_outcomes.get() + 1);
                    async { Ok(Some(wire_ineligible(true))) }.boxed_local()
                }))
        }),
        Default::default(),
    )
    .unwrap();
    let (mut raw, provider, _) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let connection = host
        .accept_client(provider, json!("user"), options)
        .unwrap();
    send(
        &raw,
        json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,"peer":{"role":"client"},"receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,"state_window_bytes":1}}}),
    );
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next(&driver, &mut raw)["t"], "hello");
    send(
        &raw,
        json!({"t":"hold","hold":1,"target":"server","consumer":contract.descriptor(),"input":{},"state":true}),
    );
    let initial = next(&driver, &mut raw);
    let (cell, other) = cells.borrow().as_ref().unwrap().clone();
    Fixture {
        driver,
        host,
        raw,
        connection,
        cell,
        other,
        outcomes,
        initial,
        contract,
    }
}

#[test]
fn incompatible_browser_contract_requires_upgrade_and_provider_revocation_is_observable() {
    let mut fixture = fixture();
    let first = next(&fixture.driver, &mut fixture.raw);
    let second = next(&fixture.driver, &mut fixture.raw);
    ack(&fixture.raw, &first);
    ack(&fixture.raw, &second);
    fixture.driver.run_until_stalled();
    let mut descriptor = fixture.contract.descriptor().clone();
    descriptor.version = 2;
    descriptor.fingerprint.clear();
    let incompatible = Contract::from_descriptor(descriptor).unwrap();
    send(
        &fixture.raw,
        json!({"t":"hold","hold":2,"target":"server","consumer":incompatible.descriptor(),"input":{},"state":false}),
    );
    let gone = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(gone["t"], "gone");
    assert_eq!(gone["hold"], 2);
    assert_eq!(gone["error"]["code"], "upgrade_required");
    fixture
        .driver
        .run_until(
            fixture
                .host
                .invalidate_principal(|principal| principal == &json!("user")),
        )
        .unwrap();
    let gone = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(gone["t"], "gone");
    assert_eq!(gone["hold"], 1);
    fixture.driver.run_until(fixture.host.close()).unwrap();
}

#[test]
fn acknowledgement_cannot_name_an_unexposed_member_or_future_revision() {
    for future_member in [false, true] {
        let mut fixture = fixture();
        let snapshot = next(&fixture.driver, &mut fixture.raw);
        let _other = next(&fixture.driver, &mut fixture.raw);
        let mut key = snapshot["key"].clone();
        if future_member {
            key["member"] = json!("ping");
        }
        send(
            &fixture.raw,
            json!({"t":"state_ack","key":key,"revision":999}),
        );
        let reason = fixture.driver.run_until(fixture.connection.closed());
        assert_eq!(reason.error.code, tembra_core::ErrorCode::Invalid);
        fixture.driver.run_until(fixture.host.close()).unwrap();
    }
}

#[test]
fn repeated_failed_reset_is_bounded_by_consecutive_recovery_admission() {
    let mut fixture = fixture();
    let initial = next(&fixture.driver, &mut fixture.raw);
    let other = next(&fixture.driver, &mut fixture.raw);
    ack(&fixture.raw, &initial);
    ack(&fixture.raw, &other);
    fixture.driver.run_until_stalled();
    write(&fixture, 1);
    let mut failed = next(&fixture.driver, &mut fixture.raw);
    for _ in 0..2 {
        let revision = failed.get("snapshot").map_or_else(
            || failed["batch"]["result_revision"].clone(),
            |snapshot| snapshot["revision"].clone(),
        );
        send(
            &fixture.raw,
            json!({"t":"resync","key":failed["key"],"failed_revision":revision,"seen_revision":0}),
        );
        failed = next(&fixture.driver, &mut fixture.raw);
        assert_eq!(failed["t"], "reset");
    }
    send(
        &fixture.raw,
        json!({"t":"resync","key":failed["key"],"failed_revision":failed["snapshot"]["revision"],"seen_revision":0}),
    );
    assert_eq!(
        fixture
            .driver
            .run_until(fixture.connection.closed())
            .error
            .reason,
        "state_recovery"
    );
    fixture.driver.run_until(fixture.host.close()).unwrap();
}
fn write(fixture: &Fixture, value: i64) {
    let cell = fixture.cell.clone();
    fixture
        .driver
        .run_until(fixture.host.root(move |ctx| {
            async move {
                cell.replace(&ctx, json!(value), ReplaceOptions::default())?;
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
}

#[test]
fn initial_fifo_and_large_single_frame_credit_allow_results_during_coalescing() {
    let mut fixture = fixture();
    let first = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(first["t"], "snapshot");
    assert_eq!(first["key"]["member"], "a");
    ack(&fixture.raw, &first);
    fixture.driver.run_until_stalled();
    write(&fixture, 1);
    let second = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(second["t"], "snapshot");
    assert_eq!(second["key"]["member"], "b");
    ack(&fixture.raw, &second);
    fixture.driver.run_until_stalled();
    let delta = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(delta["t"], "delta");
    assert_eq!(delta["batch"]["base_revision"], 0);
    // Nominal window is one byte: empty-window escape admits the bounded full frame.
    for value in 2..=20 {
        write(&fixture, value);
    }
    send(
        &fixture.raw,
        json!({"t":"call","call":1,"hold":1,"member":"ping","args":{"presence":"absent"}}),
    );
    let result = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(result["t"], "result");
    assert_eq!(result["outcome"]["data"]["value"], 7);
    ack(&fixture.raw, &delta);
    fixture.driver.run_until_stalled();
    let latest = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(latest["t"], "delta");
    assert_eq!(latest["batch"]["base_revision"], 1);
    assert_eq!(latest["batch"]["result_revision"], 20);
    ack(&fixture.raw, &latest);
    fixture.driver.run_until(fixture.host.close()).unwrap();
}

#[test]
fn negative_ack_rotates_lineage_and_stale_ack_cannot_free_current_frontier() {
    let mut fixture = fixture();
    let initial = next(&fixture.driver, &mut fixture.raw);
    let other = next(&fixture.driver, &mut fixture.raw);
    ack(&fixture.raw, &initial);
    ack(&fixture.raw, &other);
    fixture.driver.run_until_stalled();
    write(&fixture, 1);
    let failed = next(&fixture.driver, &mut fixture.raw);
    send(
        &fixture.raw,
        json!({"t":"resync","key":failed["key"],"failed_revision":failed["batch"]["result_revision"],"seen_revision":0}),
    );
    let reset = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(reset["t"], "reset");
    assert!(reset["key"]["stream"].as_u64().unwrap() > failed["key"]["stream"].as_u64().unwrap());
    ack(&fixture.raw, &failed);
    send(
        &fixture.raw,
        json!({"t":"resync","key":failed["key"],"failed_revision":failed["batch"]["result_revision"],"seen_revision":0}),
    );
    fixture.driver.run_until_stalled();
    write(&fixture, 2);
    send(
        &fixture.raw,
        json!({"t":"call","call":1,"hold":1,"member":"ping","args":{"presence":"absent"}}),
    );
    assert_eq!(next(&fixture.driver, &mut fixture.raw)["t"], "result");
    ack(&fixture.raw, &reset);
    let update = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(update["t"], "delta");
    assert_eq!(update["key"], reset["key"]);
    assert_eq!(update["batch"]["base_revision"], 1);
    assert_eq!(update["batch"]["result_revision"], 2);
    assert_eq!(
        fixture.connection.status(),
        tembra_core::rpc::ConnectionStatus::Open
    );
    fixture.driver.run_until(fixture.host.close()).unwrap();
}

fn wire_ineligible(deep: bool) -> Value {
    if deep {
        (0..127).fold(Value::Null, |value, _| json!([value]))
    } else {
        json!("x".repeat(1_048_576 - 16))
    }
}

#[test]
fn unencodable_result_is_a_typed_call_refusal_without_ending_peer_calls_or_holds() {
    let mut fixture = fixture();
    let first = next(&fixture.driver, &mut fixture.raw);
    let second = next(&fixture.driver, &mut fixture.raw);
    ack(&fixture.raw, &first);
    ack(&fixture.raw, &second);
    for (index, member) in ["large", "deep", "ping"].into_iter().enumerate() {
        send(
            &fixture.raw,
            json!({"t":"call", "call": index + 1, "hold":1,
            "member": member, "args":{"presence":"absent"}}),
        );
        let result = next(&fixture.driver, &mut fixture.raw);
        assert_eq!(result["t"], "result");
        if member == "ping" {
            assert_eq!(result["outcome"]["data"]["value"], 7);
        } else {
            assert_eq!(result["outcome"]["status"], "error");
            assert_eq!(
                result["outcome"]["data"]["reason"],
                if member == "deep" {
                    "frame_profile"
                } else {
                    "frame_limit"
                }
            );
        }
        assert_eq!(
            fixture.connection.status(),
            tembra_core::rpc::ConnectionStatus::Open
        );
    }
    assert_eq!(
        fixture.outcomes.get(),
        2,
        "result refusal does not roll back handler effects"
    );
    fixture.driver.run_until(fixture.host.close()).unwrap();
}

#[test]
fn unencodable_later_state_ends_only_that_hold_with_its_typed_error() {
    for deep in [false, true] {
        let mut fixture = fixture_schema(json!({}));
        let first = next(&fixture.driver, &mut fixture.raw);
        let second = next(&fixture.driver, &mut fixture.raw);
        ack(&fixture.raw, &first);
        ack(&fixture.raw, &second);
        send(
            &fixture.raw,
            json!({"t":"hold", "hold":2, "target":"server",
            "consumer":fixture.contract.descriptor(), "input":{}, "state":false}),
        );
        assert_eq!(next(&fixture.driver, &mut fixture.raw)["t"], "shape");
        let cell = fixture.cell.clone();
        fixture
            .driver
            .run_until(fixture.host.root(move |ctx| {
                async move {
                    cell.replace(&ctx, wire_ineligible(deep), ReplaceOptions::default())?;
                    Ok(())
                }
                .boxed_local()
            }))
            .unwrap();
        let gone = next(&fixture.driver, &mut fixture.raw);
        assert_eq!(gone["t"], "gone");
        assert_eq!(gone["hold"], 1);
        assert_eq!(
            gone["error"]["reason"],
            if deep { "frame_profile" } else { "frame_limit" }
        );
        send(
            &fixture.raw,
            json!({"t":"call", "call":1, "hold":2, "member":"ping", "args":{"presence":"absent"}}),
        );
        assert_eq!(
            next(&fixture.driver, &mut fixture.raw)["outcome"]["data"]["value"],
            7
        );
        assert_eq!(
            fixture.connection.status(),
            tembra_core::rpc::ConnectionStatus::Open
        );
        fixture.driver.run_until(fixture.host.close()).unwrap();
    }
}

#[test]
fn a_continuously_dirty_member_cannot_reclaim_every_single_frame_credit() {
    let mut fixture = fixture();
    let first = next(&fixture.driver, &mut fixture.raw);
    let second = next(&fixture.driver, &mut fixture.raw);
    ack(&fixture.raw, &first);
    ack(&fixture.raw, &second);
    fixture.driver.run_until_stalled();
    write(&fixture, 1);
    let outstanding = next(&fixture.driver, &mut fixture.raw);
    write(&fixture, 2);
    let other = fixture.other.clone();
    fixture
        .driver
        .run_until(fixture.host.root(move |ctx| {
            async move {
                other.replace(&ctx, json!(11), ReplaceOptions::default())?;
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    ack(&fixture.raw, &outstanding);
    let selected = next(&fixture.driver, &mut fixture.raw);
    let hot = selected["key"]["member"].as_str().unwrap();
    let cold = if hot == "a" { "b" } else { "a" };
    let cell = if hot == "a" {
        fixture.cell.clone()
    } else {
        fixture.other.clone()
    };
    fixture
        .driver
        .run_until(fixture.host.root(move |ctx| {
            async move {
                cell.replace(&ctx, json!(99), ReplaceOptions::default())?;
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    ack(&fixture.raw, &selected);
    let progressed = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(
        progressed["key"]["member"], cold,
        "the just-sent hot stream cannot take all renewed credit"
    );
    fixture.driver.run_until(fixture.host.close()).unwrap();
}

#[test]
fn configured_recovery_threshold_counts_failures_and_successful_ack_starts_a_new_streak() {
    let mut options = ConnectionOptions::default();
    options.limits.recoveries = 2;
    let mut fixture = fixture_options(json!({"type":"integer"}), options);
    let first = next(&fixture.driver, &mut fixture.raw);
    let second = next(&fixture.driver, &mut fixture.raw);
    ack(&fixture.raw, &first);
    ack(&fixture.raw, &second);
    fixture.driver.run_until_stalled();
    for value in [1, 2] {
        write(&fixture, value);
        let failed = next(&fixture.driver, &mut fixture.raw);
        send(
            &fixture.raw,
            json!({"t":"resync","key":failed["key"],"failed_revision":failed["batch"]["result_revision"],"seen_revision":0}),
        );
        let reset = next(&fixture.driver, &mut fixture.raw);
        assert_eq!(reset["t"], "reset", "first failure permits one Reset retry");
        if value == 1 {
            ack(&fixture.raw, &reset);
            fixture.driver.run_until_stalled();
        } else {
            send(
                &fixture.raw,
                json!({"t":"resync","key":reset["key"],"failed_revision":reset["snapshot"]["revision"],"seen_revision":0}),
            );
            assert_eq!(
                fixture
                    .driver
                    .run_until(fixture.connection.closed())
                    .error
                    .reason,
                "state_recovery"
            );
        }
    }
    fixture.driver.run_until(fixture.host.close()).unwrap();
}

#[test]
fn a_queue_with_zero_state_capacity_still_admits_method_only_holds_and_calls() {
    let mut options = ConnectionOptions::default();
    options.limits.queued_frames = 1;
    let mut fixture = fixture_options(json!({"type":"integer"}), options);
    assert_eq!(fixture.initial["t"], "gone");
    assert_eq!(fixture.initial["error"]["reason"], "unit_limit");
    send(
        &fixture.raw,
        json!({"t":"hold","hold":2,"target":"server","consumer":fixture.contract.descriptor(),"input":{},"state":false}),
    );
    let shape = next(&fixture.driver, &mut fixture.raw);
    assert_eq!(shape["t"], "shape");
    assert_eq!(shape["hold"], 2);
    send(
        &fixture.raw,
        json!({"t":"call","call":1,"hold":2,"member":"ping","args":{"presence":"absent"}}),
    );
    assert_eq!(
        next(&fixture.driver, &mut fixture.raw)["outcome"]["data"]["value"],
        7
    );
    assert_eq!(
        fixture.connection.status(),
        tembra_core::rpc::ConnectionStatus::Open
    );
    fixture.driver.run_until(fixture.host.close()).unwrap();
}

fn available(driver: &ManualDriver, link: &mut Link) -> Vec<Value> {
    driver.run_until_stalled();
    let mut messages = Vec::new();
    while let Some(Some(event)) = link.events.next().now_or_never() {
        match event {
            LinkEvent::Message(bytes) => messages.push(
                tembra_core::contracts::parse_json(std::str::from_utf8(&bytes).unwrap()).unwrap(),
            ),
            LinkEvent::Opened | LinkEvent::Writable => {}
            LinkEvent::Closed(reason) => panic!("unexpected closed link: {reason:?}"),
        }
    }
    messages
}

#[test]
fn release_and_unsubscribe_return_credit_to_other_holds_without_an_unrelated_trigger() {
    for operation in ["release", "unsubscribe"] {
        let mut fixture = fixture();
        let first = next(&fixture.driver, &mut fixture.raw);
        let second = next(&fixture.driver, &mut fixture.raw);
        ack(&fixture.raw, &first);
        ack(&fixture.raw, &second);
        fixture.driver.run_until_stalled();
        send(
            &fixture.raw,
            json!({"t":"hold","hold":2,"target":"server","consumer":fixture.contract.descriptor(),"input":{},"state":true}),
        );
        assert_eq!(next(&fixture.driver, &mut fixture.raw)["t"], "shape");
        assert_eq!(next(&fixture.driver, &mut fixture.raw)["t"], "snapshot");
        assert_eq!(next(&fixture.driver, &mut fixture.raw)["t"], "snapshot");
        // Hold 2's unacknowledged initial unit blocks all one-frame-window updates.
        write(&fixture, 1);
        assert!(available(&fixture.driver, &mut fixture.raw).is_empty());
        send(&fixture.raw, json!({"t":operation,"hold":2}));
        let messages = available(&fixture.driver, &mut fixture.raw);
        assert!(
            messages
                .iter()
                .any(|frame| frame["t"] == "delta" && frame["key"]["hold"] == 1),
            "{operation} returned state credit but failed to schedule the other dirty hold: {messages:?}"
        );
        assert_eq!(
            fixture.connection.status(),
            tembra_core::rpc::ConnectionStatus::Open
        );
        fixture.driver.run_until(fixture.host.close()).unwrap();
    }
}
