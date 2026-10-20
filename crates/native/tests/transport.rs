use std::{cell::Cell, future::Future, rc::Rc, time::Duration};

use futures::{FutureExt, SinkExt, StreamExt, channel::oneshot};
use serde_json::json;
use tembra_core::{
    Driver,
    rpc::{CloseReason, LinkEvent, SendError},
};
use tembra_native::{TokioDriver, WebSocketLink, WebSocketOptions, random_incarnation};
use tokio::{io::DuplexStream, runtime::Builder};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};

fn run<F: Future>(driver: &TokioDriver, future: F) -> F::Output {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(2), future)
                .await
                .expect("local transport must make progress")
        }))
}

fn limits() -> WebSocketOptions {
    WebSocketOptions {
        max_frame_bytes: 128,
        incoming_bytes: 256,
        incoming_frames: 2,
        outgoing_bytes: 128,
        outgoing_frames: 1,
        pre_auth_bytes: 128,
        pre_auth_frames: 1,
        auth_timeout: Duration::from_millis(100),
    }
}

async fn sockets(
    options: &WebSocketOptions,
) -> (WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
    let (server, client) = tokio::io::duplex(4096);
    let config = options.protocol_config().unwrap();
    let server = WebSocketStream::from_raw_socket(server, Role::Server, Some(config)).await;
    let client = WebSocketStream::from_raw_socket(client, Role::Client, Some(config)).await;
    (server, client)
}

#[test]
fn local_driver_reenters_spawn_and_constructs_timers_lazily() {
    let driver = TokioDriver::new();
    let sleeper = driver.sleep(Duration::from_millis(2));
    let (send, receive) = oneshot::channel();
    let nested = driver.clone();
    let value = Rc::new(Cell::new(0));
    let written = value.clone();
    driver.spawn(
        async move {
            written.set(1);
            nested.spawn(
                async move {
                    send.send(()).unwrap();
                }
                .boxed_local(),
            );
        }
        .boxed_local(),
    );
    run(&driver, async {
        receive.await.unwrap();
        sleeper.await;
        assert_eq!(value.get(), 1);
        assert!(driver.now() >= Duration::from_millis(2));
    });
    let first = random_incarnation().unwrap();
    let second = random_incarnation().unwrap();
    assert_eq!(first.len(), 32);
    assert!(
        first
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_ne!(first, second);
}

#[test]
fn outgoing_admission_is_atomic_and_capacity_returns_after_an_empty_frame() {
    let driver = TokioDriver::new();
    run(&driver, async {
        let options = limits();
        let (server, mut client) = sockets(&options).await;
        let mut link = WebSocketLink::attach(server, driver.clone(), options).unwrap();
        assert!(matches!(link.events.next().await, Some(LinkEvent::Opened)));
        link.tx.try_send(Rc::from(&b""[..])).unwrap();
        assert_eq!(
            link.tx.try_send(Rc::from(&b"refused"[..])),
            Err(SendError::WouldBlock)
        );
        assert_eq!(
            link.tx.try_send(Rc::from(vec![b'x'; 129])),
            Err(SendError::TooLarge)
        );
        assert_eq!(
            client.next().await.unwrap().unwrap().into_text().unwrap(),
            ""
        );
        assert!(matches!(
            link.events.next().await,
            Some(LinkEvent::Writable)
        ));
        link.tx.try_send(Rc::from(&b"accepted"[..])).unwrap();
        assert_eq!(
            client.next().await.unwrap().unwrap().into_text().unwrap(),
            "accepted"
        );
        link.tx.close(CloseReason::closed());
        assert_eq!(
            link.tx.try_send(Rc::from(&b"late"[..])),
            Err(SendError::Closed)
        );
        assert!(matches!(
            link.events.next().await,
            Some(LinkEvent::Closed(_))
        ));
        assert!(link.events.next().await.is_none());
    });
}

#[test]
fn input_backpressure_preserves_fifo_and_peer_disconnect_is_terminal_once() {
    let driver = TokioDriver::new();
    run(&driver, async {
        let options = limits();
        let (server, mut client) = sockets(&options).await;
        let mut link = WebSocketLink::attach(server, driver.clone(), options).unwrap();
        assert!(matches!(link.events.next().await, Some(LinkEvent::Opened)));
        client.send(Message::Text("first".into())).await.unwrap();
        client.send(Message::Text("second".into())).await.unwrap();
        for expected in [b"first".as_slice(), b"second".as_slice()] {
            match link.events.next().await.unwrap() {
                LinkEvent::Message(bytes) => assert_eq!(bytes, expected),
                other => panic!("expected FIFO text, got {other:?}"),
            }
        }
        drop(client);
        assert!(matches!(
            link.events.next().await,
            Some(LinkEvent::Closed(_))
        ));
        assert!(link.events.next().await.is_none());
    });
}

#[test]
fn authentication_retains_early_text_and_stamps_only_embedding_principal() {
    let driver = TokioDriver::new();
    run(&driver, async {
        let options = limits();
        let (server, mut client) = sockets(&options).await;
        client.send(Message::Text("early".into())).await.unwrap();
        let (mut link, stamped) =
            WebSocketLink::authenticate(server, driver.clone(), options, async {
                tokio::time::sleep(Duration::from_millis(2)).await;
                Ok(json!("trusted-embedding"))
            })
            .await
            .unwrap();
        assert_eq!(stamped, json!("trusted-embedding"));
        assert!(matches!(link.events.next().await, Some(LinkEvent::Opened)));
        assert!(
            matches!(link.events.next().await, Some(LinkEvent::Message(bytes)) if bytes == b"early")
        );
        drop(client);
        assert!(matches!(
            link.events.next().await,
            Some(LinkEvent::Closed(_))
        ));
    });
}

#[test]
fn authentication_overflow_and_deadline_admit_no_link() {
    let driver = TokioDriver::new();
    run(&driver, async {
        let options = limits();
        let (server, mut client) = sockets(&options).await;
        let (stop, stopped) = oneshot::channel();
        driver.spawn(
            async move {
                client.send(Message::Text("one".into())).await.unwrap();
                client.send(Message::Text("two".into())).await.unwrap();
                stopped.await.unwrap();
            }
            .boxed_local(),
        );
        let result = WebSocketLink::authenticate(
            server,
            driver.clone(),
            options.clone(),
            futures::future::pending(),
        )
        .await;
        let Err(error) = result else {
            panic!("overflow cannot expose a Link")
        };
        assert_eq!(error.reason, "transport_overflow");
        stop.send(()).unwrap();
        let (server, _client) = sockets(&options).await;
        let result = WebSocketLink::authenticate(
            server,
            driver.clone(),
            options,
            futures::future::pending(),
        )
        .await;
        let Err(error) = result else {
            panic!("timeout cannot expose a Link")
        };
        assert_eq!(error.reason, "auth_timeout");
    });
}

#[test]
fn binary_input_and_unbounded_socket_configuration_are_refused() {
    let driver = TokioDriver::new();
    run(&driver, async {
        let options = limits();
        let (server, mut client) = sockets(&options).await;
        let mut link = WebSocketLink::attach(server, driver.clone(), options.clone()).unwrap();
        assert!(matches!(link.events.next().await, Some(LinkEvent::Opened)));
        client
            .send(Message::Binary(vec![0, 1].into()))
            .await
            .unwrap();
        match link.events.next().await.unwrap() {
            LinkEvent::Closed(reason) => assert_eq!(reason.error.reason, "transport_binary"),
            other => panic!("binary frame cannot enter RPC, got {other:?}"),
        }
        assert!(link.events.next().await.is_none());
        let (socket, _peer) = tokio::io::duplex(1024);
        let socket = WebSocketStream::from_raw_socket(socket, Role::Server, None).await;
        let Err(error) = WebSocketLink::attach(socket, driver.clone(), options) else {
            panic!("unbounded upgrade must be refused")
        };
        assert_eq!(error.reason, "transport_limits");
    });
}

#[test]
fn oversized_text_closes_without_delivery_and_dropped_events_close_the_socket() {
    let driver = TokioDriver::new();
    run(&driver, async {
        let options = limits();
        let (server, mut client) = sockets(&options).await;
        let mut link = WebSocketLink::attach(server, driver.clone(), options.clone()).unwrap();
        assert!(matches!(link.events.next().await, Some(LinkEvent::Opened)));
        client
            .send(Message::Text("x".repeat(129).into()))
            .await
            .unwrap();
        assert!(matches!(
            link.events.next().await,
            Some(LinkEvent::Closed(_))
        ));
        assert!(link.events.next().await.is_none());
        let (server, mut client) = sockets(&options).await;
        let link = WebSocketLink::attach(server, driver.clone(), options).unwrap();
        let sender = link.tx.clone();
        drop(link.events);
        assert_eq!(
            sender.try_send(Rc::from(&b"never queued"[..])),
            Err(SendError::Closed)
        );
        assert!(matches!(client.next().await, Some(Ok(Message::Close(_)))));
        assert_eq!(
            sender.try_send(Rc::from(&b"still closed"[..])),
            Err(SendError::Closed)
        );
    });
}

#[test]
fn protocol_upgrade_category_survives_websocket_close_without_a_bye_frame() {
    use tembra_core::{ErrorCode, RuntimeError};
    use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

    let driver = TokioDriver::new();
    run(&driver, async {
        let options = limits();
        let (server, mut client) = sockets(&options).await;
        let mut link = WebSocketLink::attach(server, driver.clone(), options.clone()).unwrap();
        assert!(matches!(link.events.next().await, Some(LinkEvent::Opened)));
        link.tx.close(CloseReason::new(RuntimeError::new(
            ErrorCode::UpgradeRequired,
            "protocol_version",
            "a deliberately long diagnostic remains outside the WebSocket close reason".repeat(8),
        )));
        match client.next().await.unwrap().unwrap() {
            Message::Close(Some(frame)) => {
                assert_eq!(frame.code, CloseCode::Library(4001));
                assert!(frame.reason.len() <= 123);
            }
            other => panic!("expected a categorized close frame, got {other:?}"),
        }
        let (server, mut client) = sockets(&options).await;
        let mut link = WebSocketLink::attach(server, driver.clone(), options).unwrap();
        assert!(matches!(link.events.next().await, Some(LinkEvent::Opened)));
        client
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Library(4001),
                reason: "upgrade".into(),
            })))
            .await
            .unwrap();
        match link.events.next().await.unwrap() {
            LinkEvent::Closed(reason) => assert_eq!(reason.error.code, ErrorCode::UpgradeRequired),
            other => panic!("expected preserved upgrade category, got {other:?}"),
        }
    });
}

#[test]
fn peer_policy_close_is_typed_terminal_in_either_socket_direction_without_bye() {
    use tembra_core::ErrorCode;
    use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

    let driver = TokioDriver::new();
    run(&driver, async {
        for attach_server in [true, false] {
            for code in [CloseCode::Policy, CloseCode::Library(4008)] {
                let options = limits();
                let (server, client) = sockets(&options).await;
                let (socket, mut peer) = if attach_server {
                    (server, client)
                } else {
                    (client, server)
                };
                let mut link = WebSocketLink::attach(socket, driver.clone(), options).unwrap();
                assert!(matches!(link.events.next().await, Some(LinkEvent::Opened)));
                peer.send(Message::Close(Some(CloseFrame {
                    code,
                    reason: "untrusted peer diagnostic does not supply a RuntimeError".into(),
                })))
                .await
                .unwrap();
                let Some(LinkEvent::Closed(reason)) = link.events.next().await else {
                    panic!("policy close must produce one typed terminal event")
                };
                assert_eq!(reason.error.code, ErrorCode::Forbidden);
                assert_eq!(reason.error.reason, "policy");
                assert_eq!(reason.error.message, "peer refused connection by policy");
                assert!(link.events.next().await.is_none());
                assert_eq!(
                    link.tx.try_send(Rc::from(&b"after close"[..])),
                    Err(SendError::Closed)
                );
            }
        }
    });
}
