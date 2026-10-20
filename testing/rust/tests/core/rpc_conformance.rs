use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};

use futures::{FutureExt, StreamExt, channel::oneshot};
use serde_json::{Value, json};
use tembra_core::{
    Driver, ErrorCode, GetOptions, Host, HostOptions, Implementation, ManualDriver, RefStatus,
    ResourceDeclaration, ServeOptions,
    contracts::{Contract, Member, Placement},
    rpc::{
        CloseReason, ConnectionOptions, ConnectionStatus, Link, LinkEvent, MemoryLinkOptions,
        RootOrigin, memory_link,
    },
};

fn host(name: &str, incarnation: &str, driver: &Rc<ManualDriver>) -> Host {
    let mut options = HostOptions::new(name, json!("shared-principal"));
    options.ready_timeout = Duration::from_millis(40);
    let host = Host::new(options, driver.clone());
    host.set_rpc_identity(RootOrigin::new(name, incarnation).unwrap())
        .unwrap();
    host
}

fn contract(name: &str, target: &str) -> Rc<Contract> {
    Rc::new(
        Contract::remote(
            name,
            1,
            json!({}),
            Placement::Named {
                name: target.into(),
            },
            BTreeMap::from([("default".into(), BTreeMap::new())]),
        )
        .unwrap(),
    )
}

#[test]
fn declared_reference_cycle_across_hosts_times_out_without_claiming_global_cycle_detection() {
    let driver = ManualDriver::new();
    let first = host("first", "11111111111111111111111111111111", &driver);
    let second = host("second", "22222222222222222222222222222222", &driver);
    let first_contract = contract("conformance.cycle.first", "first");
    let second_contract = contract("conformance.cycle.second", "second");
    let factories = [Rc::new(Cell::new(0)), Rc::new(Cell::new(0))];
    let opened = [Rc::new(Cell::new(0)), Rc::new(Cell::new(0))];
    let closed = [Rc::new(Cell::new(0)), Rc::new(Cell::new(0))];
    for (index, serving, provided, requested) in [
        (0, &first, first_contract.clone(), second_contract.clone()),
        (1, &second, second_contract, first_contract.clone()),
    ] {
        let (factories, opened, closed) = (
            factories[index].clone(),
            opened[index].clone(),
            closed[index].clone(),
        );
        serving
            .serve(
                provided,
                Rc::new(move |_, principal, declaration| {
                    assert_eq!(principal, json!("shared-principal"));
                    factories.set(factories.get() + 1);
                    let opened = opened.clone();
                    let closed = closed.clone();
                    declaration.resource(
                        "activation",
                        ResourceDeclaration::new(move |_| {
                            opened.set(opened.get() + 1);
                            async { Ok(()) }.boxed_local()
                        })
                        .with_close(move |_| {
                            closed.set(closed.get() + 1);
                            async { Ok(()) }.boxed_local()
                        }),
                    )?;
                    declaration.get(requested.clone(), json!({}), GetOptions::default())?;
                    Ok(Implementation::new().variant("default"))
                }),
                ServeOptions::default(),
            )
            .unwrap();
    }
    let (left, right, control) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let down = first
        .accept_child(
            left,
            "second",
            json!("shared-principal"),
            ConnectionOptions::default(),
        )
        .unwrap();
    let up = second
        .connect_parent(
            right,
            "first",
            json!("shared-principal"),
            ConnectionOptions::default(),
        )
        .unwrap();
    first.set_route("second", down.clone()).unwrap();
    second.set_route("first", up.clone()).unwrap();
    driver
        .run_until(async { futures::try_join!(down.ready(), up.ready()) })
        .unwrap();
    let reference = first
        .hold(first_contract, json!({}), GetOptions::default())
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(reference.status(), RefStatus::Loading);
    assert_eq!(
        [factories[0].get(), factories[1].get()],
        [1, 1],
        "A→B→A coalesces the original A instead of allocating a new address"
    );
    assert_eq!([opened[0].get(), opened[1].get()], [1, 1]);
    let before = driver.now();
    let error = driver.run_until(reference.ready()).unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_eq!(error.reason, "ready_timeout");
    assert!(driver.now() - before <= Duration::from_millis(80));
    assert!(
        first
            .errors()
            .iter()
            .chain(second.errors().iter())
            .all(|error| error.reason != "cycle"),
        "a cross-host wait is not evidence of a globally detected local-graph cycle"
    );
    reference.release();
    down.close(CloseReason::closed());
    driver
        .run_until(async { futures::try_join!(first.close(), second.close()) })
        .unwrap();
    driver.run_until_stalled();
    assert_eq!(down.status(), ConnectionStatus::Closed);
    assert_eq!(up.status(), ConnectionStatus::Closed);
    assert_eq!([closed[0].get(), closed[1].get()], [1, 1]);
    assert_eq!([control.queued_bytes(0), control.queued_bytes(1)], [0, 0]);
}

fn send(raw: &Link, frame: Value) {
    raw.tx
        .try_send(Rc::from(serde_json::to_vec(&frame).unwrap()))
        .unwrap();
}

fn next_frame(driver: &ManualDriver, raw: &mut Link) -> Value {
    loop {
        match driver.run_until(raw.events.next()) {
            Some(LinkEvent::Message(bytes)) => {
                return tembra_core::contracts::parse_json(std::str::from_utf8(&bytes).unwrap())
                    .unwrap();
            }
            Some(LinkEvent::Opened | LinkEvent::Writable) => {}
            other => panic!("expected an actual encoded RPC frame, received {other:?}"),
        }
    }
}

fn method_contract(name: &str, target: &str, member: &str) -> Rc<Contract> {
    Rc::new(
        Contract::remote(
            name,
            1,
            json!({}),
            Placement::Named {
                name: target.into(),
            },
            BTreeMap::from([(
                "default".into(),
                BTreeMap::from([(
                    member.into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                )]),
            )]),
        )
        .unwrap(),
    )
}

#[test]
fn ended_own_origin_cannot_reenter_while_an_ingress_job_physically_retains_its_record() {
    let driver = ManualDriver::new();
    let origin = host("origin", "11111111111111111111111111111111", &driver);
    let work = method_contract("conformance.origin.work", "origin", "work");
    let launch = method_contract("conformance.peer.launch", "peer", "launch");
    let invocations = Rc::new(Cell::new(0));
    let finished = Rc::new(Cell::new(false));
    let closes = Rc::new(Cell::new(0));
    let (started, has_started) = oneshot::channel();
    let started = Rc::new(RefCell::new(Some(started)));
    let (finish, blocked) = oneshot::channel();
    let blocked = Rc::new(RefCell::new(Some(blocked)));
    let (invoked_factory, finished_factory, closes_factory) =
        (invocations.clone(), finished.clone(), closes.clone());
    origin
        .serve(
            work.clone(),
            Rc::new(move |_, _, declaration| {
                let (finished_close, closed) = (finished_factory.clone(), closes_factory.clone());
                let resource = declaration.resource(
                    "protected",
                    ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                        move |_| {
                            assert!(
                                finished_close.get(),
                                "an ended root cannot close resources under physical work"
                            );
                            closed.set(closed.get() + 1);
                            async { Ok(()) }.boxed_local()
                        },
                    ),
                )?;
                let (started, blocked, invoked, finished) = (
                    started.clone(),
                    blocked.clone(),
                    invoked_factory.clone(),
                    finished_factory.clone(),
                );
                Ok(Implementation::new().variant("default").json_method(
                    "work",
                    move |_, context| {
                        invoked.set(invoked.get() + 1);
                        let (started, blocked) = (
                            started.borrow_mut().take().unwrap(),
                            blocked.borrow_mut().take().unwrap(),
                        );
                        let (resource, finished) = (resource.clone(), finished.clone());
                        async move {
                            resource.value()?;
                            assert!(started.send(context.cancellation().clone()).is_ok());
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
    let (provider, mut raw, control) =
        memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
    let connection = origin
        .accept_child(
            provider,
            "peer",
            json!("shared-principal"),
            ConnectionOptions::default(),
        )
        .unwrap();
    origin.set_route("peer", connection.clone()).unwrap();
    send(
        &raw,
        json!({"t":"hello","hello":{"protocol":"tembra-rpc","version":1,"descriptor_version":1,"delta_codec":1,"peer":{"role":"host","identity":{"host":"peer","incarnation":"22222222222222222222222222222222"},"code_version":1},"receive":{"max_message_bytes":1048576,"max_unit_bytes":4194304,"state_window_bytes":262144}}}),
    );
    driver.run_until(connection.ready()).unwrap();
    assert_eq!(next_frame(&driver, &mut raw)["t"], "hello");
    send(
        &raw,
        json!({"t":"hold","hold":1,"target":"origin","consumer":work.descriptor(),"input":{},"state":false}),
    );
    assert_eq!(next_frame(&driver, &mut raw)["t"], "shape");

    let (end_owner, owner_may_end) = oneshot::channel();
    let (owner_ended, wait_owner_ended) = oneshot::channel();
    let (root_host, requested) = (origin.clone(), launch.clone());
    let expected_host = origin.id();
    driver.spawn(
        async move {
            let result = root_host
                .root(move |context| {
                    async move {
                        let reference = context
                            .get(requested, json!({}), GetOptions::default())
                            .await?;
                        let mut call =
                            Box::pin(reference.call_json("launch", None, Some(&context)));
                        assert!(futures::poll!(call.as_mut()).is_pending());
                        owner_may_end.await.unwrap();
                        context.check(expected_host)?;
                        drop(call);
                        Ok(())
                    }
                    .boxed_local()
                })
                .await;
            owner_ended.send(result).unwrap();
        }
        .boxed_local(),
    );
    let hold = next_frame(&driver, &mut raw);
    assert_eq!(hold["t"], "hold");
    send(
        &raw,
        json!({"t":"shape","hold":hold["hold"],"unit":1,"consumer_fingerprint":launch.fingerprint(),"provider":launch.descriptor(),"mode":{"mode":"direct"},"variant":"default","members":["launch"],"state":[]}),
    );
    let exported = next_frame(&driver, &mut raw);
    assert_eq!(exported["t"], "trusted_call");
    let root = exported["root"].clone();
    assert_eq!(root["identity"]["origin"]["host"], "origin");
    send(
        &raw,
        json!({"t":"trusted_call","call":1,"hold":1,"member":"work","args":{"presence":"absent"},"root":root}),
    );
    let cancellation = driver.run_until(has_started).unwrap();
    assert_eq!(invocations.get(), 1);
    send(&raw, json!({"t":"root_end","root":root["identity"]}));
    driver.run_until_stalled();
    assert!(
        !cancellation.is_cancelled(),
        "a returning carrier cannot settle the actual local owner"
    );
    end_owner.send(()).unwrap();
    driver.run_until(wait_owner_ended).unwrap().unwrap();
    assert!(cancellation.is_cancelled());
    assert!(!finished.get());
    send(
        &raw,
        json!({"t":"trusted_call","call":2,"hold":1,"member":"work","args":{"presence":"absent"},"root":root}),
    );
    let refusal = loop {
        let frame = next_frame(&driver, &mut raw);
        if frame["t"] == "result" && frame["call"] == 2 {
            break frame;
        }
    };
    assert_eq!(refusal["outcome"]["status"], "error");
    assert_eq!(refusal["outcome"]["data"]["code"], "aborted");
    assert_eq!(refusal["outcome"]["data"]["reason"], "root_ended");
    assert_eq!(
        invocations.get(),
        1,
        "a physically retained root record is not live admission authority"
    );
    send(&raw, json!({"t":"release","hold":1}));
    driver.run_until_stalled();
    assert_eq!(closes.get(), 0);
    finish.send(()).unwrap();
    loop {
        let frame = next_frame(&driver, &mut raw);
        if frame["t"] == "result" && frame["call"] == 1 {
            break;
        }
    }
    driver.run_until_stalled();
    assert!(finished.get());
    assert_eq!(closes.get(), 1);
    connection.close(CloseReason::closed());
    driver.run_until(origin.close()).unwrap();
    assert_eq!(connection.status(), ConnectionStatus::Closed);
    assert_eq!([control.queued_bytes(0), control.queued_bytes(1)], [0, 0]);
}
