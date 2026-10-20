use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    rc::Rc,
    time::Duration,
};

use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use tembra_core::{
    Driver, GetOptions, Host, HostOptions, Implementation, ResourceDeclaration, ServeOptions,
    StateEvent,
    contracts::{Contract, Member, Placement},
    rpc::{CloseReason, ConnectionOptions, Link, RootOrigin},
};
use tembra_native::{
    TokioDriver, WebSocketLink, WebSocketOptions, counter::serve_counter, random_incarnation,
};
use tokio::{net::TcpListener, runtime::Builder};
use tokio_tungstenite::{accept_async_with_config, connect_async_with_config};

fn run<F: Future>(driver: &TokioDriver, future: F) -> F::Output {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(5), future)
                .await
                .expect("real socket RPC must make progress")
        }))
}

fn host(name: &str, driver: Rc<TokioDriver>) -> Host {
    let host = Host::new(
        HostOptions::new(name, json!(format!("{name}-runtime"))),
        driver,
    );
    host.set_rpc_identity(RootOrigin::new(name, random_incarnation().unwrap()).unwrap())
        .unwrap();
    host
}

async fn links(driver: Rc<TokioDriver>) -> (Link, Link) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_driver = driver.clone();
    let server = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let options = WebSocketOptions::default();
        let socket = accept_async_with_config(stream, Some(options.protocol_config().unwrap()))
            .await
            .unwrap();
        let (link, principal) =
            WebSocketLink::authenticate(socket, server_driver, options, async {
                Ok(json!("fixture-user"))
            })
            .await
            .unwrap();
        assert_eq!(principal, json!("fixture-user"));
        link
    };
    let client = async move {
        let options = WebSocketOptions::default();
        let (socket, _) = connect_async_with_config(
            format!("ws://{address}"),
            Some(options.protocol_config().unwrap()),
            false,
        )
        .await
        .unwrap();
        WebSocketLink::attach(socket, driver, options).unwrap()
    };
    futures::join!(server, client)
}

#[test]
fn actual_tcp_websocket_uses_shared_rpc_for_counter_presence_principal_and_state_ops() {
    let driver = TokioDriver::new();
    run(&driver, async {
        let server = host("server", driver.clone());
        let client = host("client", driver.clone());
        let contract = serve_counter(&server).unwrap();
        let (server_link, client_link) = links(driver.clone()).await;
        let accepted = server
            .accept_client(
                server_link,
                json!("fixture-user"),
                ConnectionOptions::default(),
            )
            .unwrap();
        let outgoing = client
            .connect_client(client_link, "server", ConnectionOptions::default())
            .unwrap();
        client.set_route("server", outgoing.clone()).unwrap();
        futures::try_join!(accepted.ready(), outgoing.ready()).unwrap();
        let reference = client
            .get(contract, json!({}), GetOptions::default())
            .await
            .unwrap();
        let count = reference.readable("count").unwrap();
        let double = reference.readable("double").unwrap();
        assert_eq!(count.snapshot().unwrap().value(), &json!(0));
        assert_eq!(double.snapshot().unwrap().value(), &json!(0));
        let (count_ready, wait_count) = oneshot::channel();
        let count_ready = RefCell::new(Some(count_ready));
        let _count_observer = count
            .subscribe_events(move |event| {
                if event.snapshot().value() == &json!(3) {
                    assert!(matches!(event, StateEvent::Ops { .. }));
                    if let Some(ready) = count_ready.borrow_mut().take() {
                        ready.send(()).unwrap();
                    }
                }
                Ok(())
            })
            .unwrap();
        let (double_ready, wait_double) = oneshot::channel();
        let double_ready = RefCell::new(Some(double_ready));
        let _double_observer = double
            .subscribe(move |snapshot| {
                if snapshot.value() == &json!(6)
                    && let Some(ready) = double_ready.borrow_mut().take()
                {
                    ready.send(()).unwrap();
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            reference
                .call_json("add", Some(json!(3)), None)
                .await
                .unwrap(),
            Some(json!(3))
        );
        futures::try_join!(wait_count, wait_double).unwrap();
        assert_eq!(
            reference
                .call_json("nullable", Some(json!(null)), None)
                .await
                .unwrap(),
            Some(json!(null))
        );
        assert_eq!(reference.call_json("void", None, None).await.unwrap(), None);
        assert_eq!(
            reference.call_json("who", None, None).await.unwrap(),
            Some(json!("fixture-user"))
        );
        let waiting = reference.clone();
        client
            .root(move |context| {
                async move {
                    let (started, has_started) = oneshot::channel();
                    let started = RefCell::new(Some(started));
                    let _observer =
                        waiting
                            .readable("wait_started")?
                            .subscribe(move |snapshot| {
                                if snapshot.value() == &json!(true)
                                    && let Some(started) = started.borrow_mut().take()
                                {
                                    started.send(()).unwrap();
                                }
                                Ok(())
                            })?;
                    let mut call = Box::pin(waiting.call_json("wait", None, Some(&context)));
                    assert!(futures::poll!(call.as_mut()).is_pending());
                    has_started.await.unwrap();
                    context.cancel("fixture_cancel");
                    assert_eq!(
                        call.await.unwrap_err().code,
                        tembra_core::ErrorCode::Aborted
                    );
                    Ok(())
                }
                .boxed_local()
            })
            .await
            .unwrap();
        reference.release();
        outgoing.close(CloseReason::closed());
        accepted.closed().await;
        client.close().await.unwrap();
        server.close().await.unwrap();
    });
}

#[test]
fn disconnect_rejects_caller_but_keeps_provider_resource_until_physical_work_settles() {
    let driver = TokioDriver::new();
    run(&driver, async {
        let server = host("server", driver.clone());
        let client = host("client", driver.clone());
        let contract = Rc::new(
            Contract::remote(
                "transport.physical",
                1,
                json!({}),
                Placement::Named {
                    name: "server".into(),
                },
                BTreeMap::from([(
                    "default".into(),
                    BTreeMap::from([(
                        "work".into(),
                        Member::Method {
                            args: None,
                            result: None,
                        },
                    )]),
                )]),
            )
            .unwrap(),
        );
        let (started, wait_started) = oneshot::channel();
        let started = Rc::new(RefCell::new(Some(started)));
        let (complete, blocked) = oneshot::channel();
        let blocked = Rc::new(RefCell::new(Some(blocked)));
        let physically_finished = Rc::new(Cell::new(false));
        let closed = Rc::new(Cell::new(0));
        let (finished_factory, closed_factory) = (physically_finished.clone(), closed.clone());
        server
            .serve(
                contract.clone(),
                Rc::new(move |_, _, declaration| {
                    let (finished, closed) = (finished_factory.clone(), closed_factory.clone());
                    let resource = declaration.resource(
                        "work",
                        ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                            move |_| {
                                assert!(
                                    finished.get(),
                                    "logical disconnect must not close a physically used resource"
                                );
                                closed.set(closed.get() + 1);
                                async { Ok(()) }.boxed_local()
                            },
                        ),
                    )?;
                    let (started, blocked, finished) =
                        (started.clone(), blocked.clone(), finished_factory.clone());
                    Ok(Implementation::new().variant("default").json_method(
                        "work",
                        move |_, ctx| {
                            let (send, blocked) = (
                                started.borrow_mut().take().unwrap(),
                                blocked.borrow_mut().take().unwrap(),
                            );
                            let (resource, finished) = (resource.clone(), finished.clone());
                            async move {
                                resource.value()?;
                                assert!(send.send(ctx.cancellation().clone()).is_ok());
                                blocked.await.unwrap();
                                resource.value()?;
                                finished.set(true);
                                Ok(None)
                            }
                            .boxed_local()
                        },
                    ))
                }),
                ServeOptions::default(),
            )
            .unwrap();
        let (server_link, client_link) = links(driver.clone()).await;
        let accepted = server
            .accept_client(
                server_link,
                json!("fixture-user"),
                ConnectionOptions::default(),
            )
            .unwrap();
        let outgoing = client
            .connect_client(client_link, "server", ConnectionOptions::default())
            .unwrap();
        client.set_route("server", outgoing.clone()).unwrap();
        futures::try_join!(accepted.ready(), outgoing.ready()).unwrap();
        let reference = client
            .get(contract, json!({}), GetOptions::default())
            .await
            .unwrap();
        let (answered, answer) = oneshot::channel();
        let called = reference.clone();
        driver.spawn(
            async move {
                answered
                    .send(called.call_json("work", None, None).await)
                    .unwrap();
            }
            .boxed_local(),
        );
        let cancellation = wait_started.await.unwrap();
        outgoing.close(CloseReason::closed());
        let error = answer.await.unwrap().unwrap_err();
        assert_eq!(error.reason, "unknown_outcome");
        accepted.closed().await;
        assert!(cancellation.is_cancelled());
        reference.release();
        assert!(!physically_finished.get());
        assert_eq!(closed.get(), 0);
        let mut closing = Box::pin(server.close());
        assert!(futures::poll!(closing.as_mut()).is_pending());
        assert_eq!(closed.get(), 0);
        complete.send(()).unwrap();
        closing.await.unwrap();
        assert!(physically_finished.get());
        assert_eq!(closed.get(), 1);
        client.close().await.unwrap();
    });
}
