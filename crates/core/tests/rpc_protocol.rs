use std::rc::Rc;

use futures::StreamExt;
use serde_json::{Value, json};
use tembra_core::{
    ErrorCode, Host, HostOptions, ManualDriver,
    rpc::{
        ConnectionOptions, ConnectionStatus, LinkEvent, MemoryLinkOptions, RootOrigin, memory_link,
    },
};

fn host(driver: Rc<ManualDriver>, name: &str) -> Host {
    let host = Host::new(HostOptions::new(name, Value::Null), driver);
    host.set_rpc_identity(RootOrigin::new(name, "00000000000000000000000000000001").unwrap())
        .unwrap();
    host
}
fn hello() -> Rc<[u8]> {
    Rc::from(serde_json::to_vec(&json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,"peer":{"role":"client"},"receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,"state_window_bytes":262144}}})).unwrap())
}

#[test]
fn prehello_requests_and_forged_client_roots_close_without_execution() {
    for malicious in [
        br#"{"t":"hold","hold":1,"target":"server","consumer":{},"input":{},"state":false}"#.as_slice(),
        br#"{"t":"root_end","root":{"origin":{"host":"forged","incarnation":"00000000000000000000000000000002"},"sequence":1}}"#.as_slice(),
    ] {
        let driver = ManualDriver::new();
        let server = host(driver.clone(), "server");
        let (raw, provider, _) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
        let connection = server.accept_client(provider, json!("authenticated"), ConnectionOptions::default()).unwrap();
        if malicious.windows(8).any(|part| part == b"root_end") {
            raw.tx.try_send(hello()).unwrap();
            driver.run_until(connection.ready()).unwrap();
        }
        raw.tx.try_send(Rc::from(malicious)).unwrap();
        let reason = driver.run_until(connection.closed());
        assert_eq!(connection.status(), ConnectionStatus::Closed);
        if malicious.windows(8).any(|part| part == b"root_end") {
            assert_eq!(reason.error.code, ErrorCode::Forbidden);
            assert_eq!(reason.error.reason, "rpc_role");
        } else {
            assert_eq!(reason.error.code, ErrorCode::Unavailable);
            assert_eq!(reason.error.reason, "connection");
        }
        driver.run_until(server.close()).unwrap();
    }
}

#[test]
fn mismatch_keeps_upgrade_error_and_duplicate_json_never_reaches_policy() {
    for bytes in [
        br#"{"t":"hello","hello":{"protocol":"tembra-rpc","version":2,"descriptor_version":1,"delta_codec":1,"peer":{"role":"client"},"receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,"state_window_bytes":262144}}}"#.as_slice(),
        br#"{"t":"ping","nonce":1,"nonce":2}"#.as_slice(),
    ] {
        let driver = ManualDriver::new();
        let server = host(driver.clone(), "server");
        let (raw, provider, _) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
        let connection = server.accept_client(provider, Value::Null, ConnectionOptions::default()).unwrap();
        raw.tx.try_send(Rc::from(bytes)).unwrap();
        let reason = driver.run_until(connection.closed());
        if bytes.windows(11).any(|part| part == b"\"version\":2") { assert_eq!(reason.error.code, ErrorCode::UpgradeRequired); }
        else { assert_eq!(reason.error.code, ErrorCode::Invalid); }
        driver.run_until(server.close()).unwrap();
    }
}

#[test]
fn memory_transport_has_atomic_bounded_admission_and_fifo_terminal_event() {
    let driver = ManualDriver::new();
    let (left, mut right, control) = memory_link(
        driver.clone(),
        MemoryLinkOptions {
            max_message_bytes: 4,
            queued_bytes: 4,
            queued_messages: 1,
        },
    )
    .unwrap();
    control.pause(0, true);
    left.tx.try_send(Rc::from(&b"abcd"[..])).unwrap();
    assert_eq!(control.queued_bytes(0), 4);
    assert_eq!(
        left.tx.try_send(Rc::from(&b"x"[..])),
        Err(tembra_core::rpc::SendError::WouldBlock)
    );
    assert_eq!(
        left.tx.try_send(Rc::from(&b"large"[..])),
        Err(tembra_core::rpc::SendError::TooLarge)
    );
    control.pause(0, false);
    assert!(matches!(
        driver.run_until(right.events.next()),
        Some(LinkEvent::Opened)
    ));
    assert!(
        matches!(driver.run_until(right.events.next()), Some(LinkEvent::Message(bytes)) if bytes == b"abcd")
    );
    left.tx.close(tembra_core::rpc::CloseReason::closed());
    assert!(matches!(
        driver.run_until(right.events.next()),
        Some(LinkEvent::Closed(_))
    ));
    assert!(driver.run_until(right.events.next()).is_none());
}

#[test]
fn silent_sever_is_detected_by_shared_liveness_not_transport_close() {
    let driver = ManualDriver::new();
    let server = host(driver.clone(), "server");
    let client = host(driver.clone(), "browser");
    let (left, right, control) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let provider = server
        .accept_client(right, Value::Null, ConnectionOptions::default())
        .unwrap();
    let consumer = client
        .connect_client(left, "server", ConnectionOptions::default())
        .unwrap();
    driver.run_until(consumer.ready()).unwrap();
    driver.run_until(provider.ready()).unwrap();
    control.sever();
    let reason = driver.run_until(consumer.closed());
    assert_eq!(reason.error.reason, "liveness");
    driver.run_until(server.close()).unwrap();
    driver.run_until(client.close()).unwrap();
}

#[test]
fn outgoing_browser_profile_rejects_provider_requests() {
    let driver = ManualDriver::new();
    let browser = host(driver.clone(), "browser");
    let (consumer, provider, _) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let connection = browser
        .connect_client(consumer, "server", ConnectionOptions::default())
        .unwrap();
    provider.tx.try_send(Rc::from(serde_json::to_vec(&json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,"peer":{"role":"host","identity":{"host":"server","incarnation":"00000000000000000000000000000002"},"code_version":1},"receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,"state_window_bytes":262144}}})).unwrap())).unwrap();
    driver.run_until(connection.ready()).unwrap();
    provider
        .tx
        .try_send(Rc::from(
            &br#"{"t":"hold","hold":1,"target":"browser","consumer":{},"input":{},"state":false}"#
                [..],
        ))
        .unwrap();
    assert_eq!(
        driver.run_until(connection.closed()).error.code,
        ErrorCode::Forbidden
    );
    driver.run_until(browser.close()).unwrap();
}

#[test]
fn always_ready_message_burst_yields_to_another_local_job() {
    use futures::{FutureExt, stream};
    use std::cell::Cell;
    use tembra_core::{
        Driver,
        rpc::{CloseReason, Link, LinkTx, SendError},
    };
    struct Probe(Rc<Cell<usize>>);
    impl LinkTx for Probe {
        fn try_send(&self, bytes: Rc<[u8]>) -> Result<(), SendError> {
            let frame: Value = serde_json::from_slice(&bytes).unwrap();
            if frame["t"] == "pong" {
                self.0.set(self.0.get() + 1);
            }
            Ok(())
        }
        fn close(&self, _: CloseReason) {}
    }
    let driver = ManualDriver::new();
    let server = host(driver.clone(), "server");
    let count = Rc::new(Cell::new(0));
    let mut events = vec![LinkEvent::Opened, LinkEvent::Message(hello().to_vec())];
    for nonce in 1..=200 {
        events.push(LinkEvent::Message(
            serde_json::to_vec(&json!({"t":"ping","nonce":nonce})).unwrap(),
        ));
    }
    let link = Link {
        tx: Rc::new(Probe(count.clone())),
        events: stream::iter(events).chain(stream::pending()).boxed_local(),
    };
    let _connection = server
        .accept_client(link, Value::Null, ConnectionOptions::default())
        .unwrap();
    let observed = Rc::new(Cell::new(None));
    let captured = observed.clone();
    let running_count = count.clone();
    driver.spawn(
        async move {
            captured.set(Some(running_count.get()));
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    let midway = observed.get().unwrap();
    assert!(
        midway > 0 && midway < 200,
        "other local work saw {midway} processed replies"
    );
    assert_eq!(count.get(), 200);
    driver.run_until(server.close()).unwrap();
}
