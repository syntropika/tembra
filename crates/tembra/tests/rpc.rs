#![cfg(feature = "rpc")]

use futures::{FutureExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tembra::{
    Code, Error, Host, Orb, Service,
    rpc::{Connection, Options, serve},
};

type Frames = Rc<RefCell<Vec<Value>>>;

fn connection(host: &Host, options: Options) -> (Rc<Connection>, Frames) {
    let frames = Rc::new(RefCell::new(Vec::new()));
    let sent = frames.clone();
    let connection = serve(host.clone(), options, move |bytes| {
        sent.borrow_mut()
            .push(serde_json::from_slice(&bytes).unwrap());
        Ok(())
    });
    (Rc::new(connection), frames)
}

fn options() -> Options {
    Options::new("reader".into(), |_, _| true)
}

fn request(connection: &Connection, id: u64, op: &str, payload: Value) {
    connection
        .receive(&serde_json::to_vec(&json!({"id": id, "op": op, "payload": payload})).unwrap())
        .unwrap();
}

fn response(frames: &Frames, id: u64) -> Value {
    frames
        .borrow()
        .iter()
        .find(|frame| frame["id"] == id)
        .unwrap_or_else(|| panic!("missing response {id}: {:?}", frames.borrow()))
        .clone()
}

fn get(connection: &Connection, id: u64, service: &str) {
    request(
        connection,
        id,
        "get",
        json!({"service": service, "key": ["main"]}),
    );
}

fn call(connection: &Connection, id: u64, reference: u64, method: &str, arguments: Value) {
    request(
        connection,
        id,
        "call",
        json!({"reference": reference, "method": method, "arguments": arguments}),
    );
}

#[test]
fn get_call_state_and_release_share_one_native_object() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("counter", 1).service(
            Service::new("counter", |declaration| {
                declaration.state(0_i32, 1, |_| Ok(()))
            })
            .method("set", |state, ctx, (value,): (i32,)| async move {
                Ok(*state.update(&ctx, |_| Ok(value))?)
            }),
        ),
    ])
    .unwrap();
    let (connection, frames) = connection(&host, options());
    get(&connection, 1, "counter");
    pool.run_until_stalled();
    let acquired = response(&frames, 1);
    let reference = acquired["ok"]["reference"].as_u64().unwrap();
    assert_eq!(acquired["ok"]["methods"], json!(["set"]));
    assert_eq!(acquired["ok"]["state"], 0);
    call(&connection, 2, reference, "set", json!([7]));
    pool.run_until_stalled();
    assert_eq!(response(&frames, 2)["ok"], 7);
    let events: Vec<_> = frames
        .borrow()
        .iter()
        .filter(|frame| frame["event"] == "state")
        .cloned()
        .collect();
    assert_eq!(
        events,
        vec![json!({"event": "state", "payload": {"reference": reference, "value": 7}})]
    );
    request(&connection, 3, "release", json!({"reference": reference}));
    pool.run_until_stalled();
    assert_eq!(response(&frames, 3)["ok"], Value::Null);
    assert_eq!(host.kernel().diagnostics().objects, 0);
    call(&connection, 4, reference, "set", json!([8]));
    pool.run_until_stalled();
    assert_eq!(response(&frames, 4)["error"]["code"], "forbidden");
    connection.close();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn reentrant_sender_mutation_publishes_the_latest_state_after_the_get_reply() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let state = Rc::new(RefCell::new(None));
    host.install(vec![
        Orb::new("counter", 1).service(
            Service::new("counter", {
                let retained = state.clone();
                move |declaration| {
                    let state = declaration.state(0_i32, 1, |_| Ok(()))?;
                    *retained.borrow_mut() = Some(state.clone());
                    Ok(state)
                }
            })
            .method("set", |state, ctx, (value,): (i32,)| async move {
                state.update(&ctx, |_| Ok(value))?;
                Ok(())
            }),
        ),
    ])
    .unwrap();
    let local = host.get("counter", vec!["main".into()], "reader").unwrap();
    pool.run_until(local.ready()).unwrap();
    let root = host.kernel().begin_root("reader").unwrap();
    let invocation = host.kernel().begin_call(local.id(), root, "set").unwrap();
    let context = host.context(root, Some(invocation.clone())).unwrap();
    let frames = Rc::new(RefCell::new(Vec::<Value>::new()));
    let sent = frames.clone();
    let connection = serve(host.clone(), options(), move |bytes| {
        let frame: Value = serde_json::from_slice(&bytes).unwrap();
        let acquired = frame["id"] == 1;
        sent.borrow_mut().push(frame);
        if acquired {
            let state = state.borrow().as_ref().unwrap().clone();
            state.update(&context, |_| Ok(7))?;
            state.update(&context, |_| Ok(8))?;
        }
        Ok(())
    });
    get(&connection, 1, "counter");
    pool.run_until_stalled();
    let acquired = response(&frames, 1);
    let reference = acquired["ok"]["reference"].as_u64().unwrap();
    assert_eq!(acquired["ok"]["state"], 0);
    assert_eq!(frames.borrow().len(), 2);
    assert_eq!(frames.borrow()[0]["id"], 1);
    assert_eq!(
        frames.borrow()[1],
        json!({"event": "state", "payload": {"reference": reference, "value": 8}})
    );
    host.kernel().finish_call(invocation.id).unwrap();
    host.kernel().end_root(root).unwrap();
    host.flush();
    local.release().unwrap();
    connection.close();
    pool.run_until(host.close()).unwrap();
}

#[cfg(feature = "views")]
#[test]
fn catalog_and_claim_methods_follow_the_authenticated_principal() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let writes = Rc::new(Cell::new(0));
    host.install(vec![
        Orb::new("restricted", 1).service(
            Service::new("restricted", {
                let writes = writes.clone();
                move |_| Ok(writes.clone())
            })
            .method("read", |_, ctx, (): ()| async move { ctx.principal() })
            .method("write", |writes, _, (): ()| async move {
                writes.set(writes.get() + 1);
                Ok(())
            })
            .access(|principal| {
                if principal == "admin" {
                    ["read".into(), "write".into()].into()
                } else {
                    ["read".into()].into()
                }
            }),
        ),
    ])
    .unwrap();
    let (connection, frames) = connection(&host, options());
    request(&connection, 1, "describe", Value::Null);
    get(&connection, 2, "restricted");
    pool.run_until_stalled();
    assert_eq!(
        response(&frames, 1)["ok"][0]["services"][0]["methods"],
        json!(["read"])
    );
    let acquired = response(&frames, 2);
    assert_eq!(acquired["ok"]["methods"], json!(["read"]));
    let reference = acquired["ok"]["reference"].as_u64().unwrap();
    request(
        &connection,
        3,
        "call",
        json!({"reference": reference, "method": "read", "arguments": [], "principal": "admin"}),
    );
    call(&connection, 4, reference, "write", json!([]));
    pool.run_until_stalled();
    assert_eq!(response(&frames, 3)["ok"], "reader");
    assert_eq!(response(&frames, 4)["error"]["code"], "forbidden");
    assert_eq!(writes.get(), 0);
    connection.close();
    pool.run_until(host.close()).unwrap();
}

#[cfg(feature = "views")]
#[test]
fn acquired_methods_use_the_granted_view_without_evaluating_access_again() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let evaluations = Rc::new(Cell::new(0));
    host.install(vec![
        Orb::new("view", 1).service(
            Service::new("view", |_| Ok(()))
                .method("read", |_, _, (): ()| async { Ok(42) })
                .method("write", |_, _, (): ()| async { Ok(()) })
                .access({
                    let evaluations = evaluations.clone();
                    move |principal| {
                        assert_eq!(principal, "reader");
                        evaluations.set(evaluations.get() + 1);
                        if evaluations.get() == 1 {
                            ["read".into()].into()
                        } else {
                            ["write".into()].into()
                        }
                    }
                }),
        ),
    ])
    .unwrap();
    let (connection, frames) = connection(&host, options());
    get(&connection, 1, "view");
    pool.run_until_stalled();
    let acquired = response(&frames, 1);
    assert_eq!(acquired["ok"]["methods"], json!(["read"]));
    let reference = acquired["ok"]["reference"].as_u64().unwrap();
    call(&connection, 2, reference, "read", json!([]));
    call(&connection, 3, reference, "write", json!([]));
    pool.run_until_stalled();
    assert_eq!(response(&frames, 2)["ok"], 42);
    assert_eq!(response(&frames, 3)["error"]["code"], "forbidden");
    assert_eq!(evaluations.get(), 1);
    connection.close();
    pool.run_until(host.close()).unwrap();
}

#[cfg(feature = "views")]
#[test]
fn method_surface_intersects_required_methods_and_the_granted_view() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("view", 1).service(
            Service::new("view", |_| Ok(()))
                .method("read", |_, _, (): ()| async { Ok(()) })
                .method("write", |_, _, (): ()| async { Ok(()) })
                .access(|_| ["read".into(), "write".into(), "undeclared".into()].into()),
        ),
    ])
    .unwrap();
    let reference = host
        .get_with_options(
            "view",
            vec!["main".into()],
            "reader",
            tembra::AcquisitionOptions {
                api_version: 1,
                methods: ["read".into()].into(),
            },
        )
        .unwrap();
    pool.run_until(reference.ready()).unwrap();
    assert_eq!(
        host.kernel().reference_methods(reference.id()).unwrap(),
        ["read".into()].into()
    );
    reference.release().unwrap();
    assert!(host.kernel().reference_methods(reference.id()).is_err());
    pool.run_until(host.close()).unwrap();
}

#[test]
fn request_and_reference_budgets_recover_after_work_is_released() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (settle, settled) = oneshot::channel();
    let settled = Rc::new(RefCell::new(Some(settled)));
    host.install(vec![Orb::new("pending", 1).service(
        Service::new("pending", move |_| Ok(settled.clone())).method(
            "wait",
            |settled, ctx, (): ()| {
                let settled = settled.borrow_mut().take().unwrap();
                async move {
                    settled.await.unwrap();
                    ctx.check()
                }
            },
        ),
    )])
    .unwrap();
    let mut limits = options();
    limits.max_pending = 1;
    limits.max_references = 1;
    let (connection, frames) = connection(&host, limits);
    get(&connection, 1, "pending");
    pool.run_until_stalled();
    let reference = response(&frames, 1)["ok"]["reference"].as_u64().unwrap();
    get(&connection, 2, "pending");
    pool.run_until_stalled();
    assert_eq!(response(&frames, 2)["error"]["reason"], "remote_references");
    call(&connection, 3, reference, "wait", json!([]));
    pool.run_until_stalled();
    connection
        .receive(br#"{"op":"cancel","payload":{"id":3}}"#)
        .unwrap();
    request(&connection, 4, "describe", Value::Null);
    pool.run_until_stalled();
    assert_eq!(response(&frames, 4)["error"]["reason"], "pending_requests");
    settle.send(()).unwrap();
    pool.run_until_stalled();
    assert_eq!(response(&frames, 3)["error"]["code"], "cancelled");
    request(&connection, 5, "release", json!({"reference": reference}));
    pool.run_until_stalled();
    get(&connection, 6, "pending");
    pool.run_until_stalled();
    assert!(response(&frames, 6)["ok"]["reference"].is_u64());
    assert!(!connection.is_closed());
    connection.close();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn exposure_policy_distinguishes_catalog_visibility_from_key_access() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("example", 1).service(Service::new("example", |_| Ok(()))),
    ])
    .unwrap();
    let policy = Options::new("reader".into(), |service, key| {
        service == "example" && key.is_none_or(|key| key == ["allowed"])
    });
    let (connection, frames) = connection(&host, policy);
    request(&connection, 1, "describe", Value::Null);
    get(&connection, 2, "example");
    request(
        &connection,
        3,
        "get",
        json!({"service": "example", "key": ["allowed"]}),
    );
    pool.run_until_stalled();
    assert_eq!(
        response(&frames, 1)["ok"][0]["services"][0]["name"],
        "example"
    );
    assert_eq!(response(&frames, 2)["error"]["code"], "forbidden");
    assert!(response(&frames, 3)["ok"]["reference"].is_u64());
    connection.close();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn malformed_input_closes_only_its_connection() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (broken, _) = connection(&host, options());
    let (healthy, frames) = connection(&host, options());
    assert!(broken.receive(b"{").is_err());
    assert!(broken.is_closed());
    assert!(broken.receive(b"{}").is_err());
    request(&healthy, 1, "describe", Value::Null);
    pool.run_until_stalled();
    assert_eq!(response(&frames, 1)["ok"], json!([]));
    assert!(!healthy.is_closed());
    healthy.close();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn close_wakes_a_waiter_without_transport_traffic() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (connection, frames) = connection(&host, options());
    let completed = Rc::new(Cell::new(false));
    let waiting = connection.clone();
    let observed = completed.clone();
    pool.spawner()
        .spawn_local(async move {
            waiting.closed().await;
            observed.set(true);
        })
        .unwrap();
    pool.run_until_stalled();
    assert!(!completed.get());
    connection.close();
    pool.run_until_stalled();
    assert!(completed.get());
    assert!(connection.is_closed());
    assert!(frames.borrow().is_empty());
    connection.close();
    pool.run_until(connection.closed());
    pool.run_until(host.close()).unwrap();
}

#[test]
fn oversized_initial_state_returns_a_bounded_error_without_retaining_a_claim() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("state", 1)
            .service(Service::new("large", |declaration| {
                declaration.state("x".repeat(1024), 1, |_| Ok(()))
            }))
            .service(Service::new("small", |_| Ok(()))),
    ])
    .unwrap();
    let mut limits = options();
    limits.max_frame = 256;
    limits.max_references = 1;
    let (connection, frames) = connection(&host, limits);
    get(&connection, 1, "large");
    pool.run_until_stalled();
    let error = response(&frames, 1);
    assert_eq!(error["error"]["code"], "exhausted");
    assert_eq!(error["error"]["reason"], "frame_size");
    assert_eq!(host.kernel().diagnostics().objects, 0);
    assert!(!connection.is_closed());
    get(&connection, 2, "small");
    pool.run_until_stalled();
    assert!(response(&frames, 2)["ok"]["reference"].is_u64());
    assert!(
        frames
            .borrow()
            .iter()
            .all(|frame| serde_json::to_vec(frame).unwrap().len() <= 256)
    );
    connection.close();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn oversized_live_state_closes_with_the_frame_limit_cause() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("state", 1).service(
            Service::new("state", |declaration| {
                declaration.state(String::new(), 1, |_| Ok(()))
            })
            .method("grow", |state, ctx, (): ()| async move {
                state.update(&ctx, |_| Ok("x".repeat(1024)))?;
                Ok(())
            }),
        ),
    ])
    .unwrap();
    let mut limits = options();
    limits.max_frame = 256;
    let (connection, frames) = connection(&host, limits);
    get(&connection, 1, "state");
    pool.run_until_stalled();
    let reference = response(&frames, 1)["ok"]["reference"].as_u64().unwrap();
    call(&connection, 2, reference, "grow", json!([]));
    pool.run_until_stalled();
    assert!(connection.is_closed());
    let cause = connection.close_cause().unwrap();
    assert_eq!(cause.code, Code::Exhausted);
    assert_eq!(cause.reason, "frame_size");
    assert_eq!(host.kernel().diagnostics().objects, 0);
    pool.run_until(host.close()).unwrap();
}

#[test]
fn oversized_method_result_reports_refusal_and_preserves_effects_and_channel_use() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let effects = Rc::new(Cell::new(0));
    host.install(vec![
        Orb::new("example", 1).service(
            Service::new("example", {
                let effects = effects.clone();
                move |_| Ok(effects.clone())
            })
            .method("large", |effects, _, (): ()| async move {
                effects.set(effects.get() + 1);
                Ok("x".repeat(1024))
            })
            .method(
                "effects",
                |effects, _, (): ()| async move { Ok(effects.get()) },
            )
            .method("read", |_, _, (): ()| async move { Ok(42) }),
        ),
    ])
    .unwrap();
    let mut limits = options();
    limits.max_frame = 256;
    limits.max_pending = 1;
    let (connection, frames) = connection(&host, limits);
    get(&connection, 1, "example");
    pool.run_until_stalled();
    let reference = response(&frames, 1)["ok"]["reference"].as_u64().unwrap();
    call(&connection, 2, reference, "large", json!([]));
    pool.run_until_stalled();
    assert_eq!(response(&frames, 2)["error"]["code"], "exhausted");
    assert_eq!(response(&frames, 2)["error"]["reason"], "frame_size");
    assert_eq!(effects.get(), 1);
    assert!(!connection.is_closed());
    call(&connection, 3, reference, "effects", json!([]));
    pool.run_until_stalled();
    assert_eq!(response(&frames, 3)["ok"], 1);
    call(&connection, 4, reference, "read", json!([]));
    pool.run_until_stalled();
    assert_eq!(response(&frames, 4)["ok"], 42);
    assert_eq!(frames.borrow().len(), 4);
    assert!(
        frames
            .borrow()
            .iter()
            .all(|frame| serde_json::to_vec(frame).unwrap().len() <= 256)
    );
    assert!(connection.close_cause().is_none());
    connection.close();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn an_unencodable_refusal_closes_without_sending_an_oversized_frame() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("example", 1).service(
            Service::new("example", |_| Ok(()))
                .method("a_method_with_a_long_name", |_, _, (): ()| async { Ok(()) }),
        ),
    ])
    .unwrap();
    let mut limits = options();
    limits.max_frame = 64;
    let (connection, frames) = connection(&host, limits);
    let request = serde_json::to_vec(&json!({"id": 1, "op": "describe"})).unwrap();
    assert!(request.len() <= 64);
    connection.receive(&request).unwrap();
    pool.run_until_stalled();
    assert!(connection.is_closed());
    let cause = connection.close_cause().unwrap();
    assert_eq!(cause.code, Code::Exhausted);
    assert_eq!(cause.reason, "frame_size");
    assert!(frames.borrow().is_empty());
    pool.run_until(connection.closed());
    pool.run_until(host.close()).unwrap();
}

#[test]
fn the_first_close_preserves_its_cause_including_explicit_closure_without_a_cause() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (explicit, explicit_frames) = connection(&host, options());
    explicit.close();
    explicit.close_with(Error::invalid("late", "late failure"));
    assert!(explicit.is_closed());
    assert!(explicit.close_cause().is_none());
    assert_eq!(explicit.receive(b"{").unwrap_err().code, Code::Gone);
    assert!(explicit.close_cause().is_none());
    let (failed, failed_frames) = connection(&host, options());
    failed.close_with(Error::new(Code::Unavailable, "first", "original failure"));
    failed.close();
    failed.close_with(Error::invalid("second", "replacement failure"));
    assert_eq!(failed.receive(b"{").unwrap_err().code, Code::Gone);
    let cause = failed.close_cause().unwrap();
    assert_eq!(cause.code, Code::Unavailable);
    assert_eq!(cause.reason, "first");
    assert_eq!(cause.message, "original failure");
    assert!(explicit_frames.borrow().is_empty());
    assert!(failed_frames.borrow().is_empty());
    pool.run_until(explicit.closed());
    pool.run_until(failed.closed());
    pool.run_until(host.close()).unwrap();
}

#[test]
fn sender_frame_size_failure_is_terminal_and_never_retries_as_a_refusal() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let attempts = Rc::new(RefCell::new(Vec::<Value>::new()));
    let sent = attempts.clone();
    let connection = serve(host.clone(), options(), move |bytes| {
        sent.borrow_mut()
            .push(serde_json::from_slice(&bytes).unwrap());
        Err(Error::new(
            Code::Exhausted,
            "frame_size",
            "transport queue full",
        ))
    });
    request(&connection, 1, "describe", Value::Null);
    pool.run_until_stalled();
    assert!(connection.is_closed());
    let cause = connection.close_cause().unwrap();
    assert_eq!(cause.code, Code::Exhausted);
    assert_eq!(cause.reason, "frame_size");
    assert_eq!(cause.message, "transport queue full");
    assert_eq!(*attempts.borrow(), vec![json!({"id": 1, "ok": []})]);
    assert!(connection.receive(br#"{"id":2,"op":"describe"}"#).is_err());
    pool.run_until_stalled();
    assert_eq!(attempts.borrow().len(), 1);
    pool.run_until(host.close()).unwrap();
}

#[test]
fn an_unknown_operation_returns_a_structured_error_and_leaves_the_channel_usable() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (connection, frames) = connection(&host, options());
    request(&connection, 1, "unsupported", Value::Null);
    pool.run_until_stalled();
    assert_eq!(response(&frames, 1)["error"]["code"], "invalid");
    assert_eq!(response(&frames, 1)["error"]["reason"], "rpc_operation");
    assert!(!connection.is_closed());
    request(&connection, 2, "describe", Value::Null);
    pool.run_until_stalled();
    assert_eq!(response(&frames, 2)["ok"], json!([]));
    assert!(connection.close_cause().is_none());
    connection.close();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn sender_failure_closes_with_the_transport_cause() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("example", 1).service(Service::new("example", |_| Ok(()))),
    ])
    .unwrap();
    let connection = serve(host.clone(), options(), |_| {
        Err(Error::invalid("transport", "sender stopped"))
    });
    get(&connection, 1, "example");
    pool.run_until_stalled();
    assert!(connection.is_closed());
    let cause = connection.close_cause().unwrap();
    assert_eq!(cause.code, Code::Invalid);
    assert_eq!(cause.reason, "transport");
    assert_eq!(host.kernel().diagnostics().objects, 0);
    pool.run_until(host.close()).unwrap();
}

#[test]
fn cancelled_acquisition_cleans_up_after_its_resource_opener_settles() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (settle, settled) = oneshot::channel();
    let settled = Rc::new(RefCell::new(Some(settled)));
    let opened = Rc::new(Cell::new(false));
    let closes = Rc::new(Cell::new(0));
    host.install(vec![Orb::new("pending", 1).service(Service::new(
        "pending",
        {
            let (opened, closes) = (opened.clone(), closes.clone());
            move |declaration| {
                let settled = settled.borrow_mut().take().unwrap();
                let (opened, closes) = (opened.clone(), closes.clone());
                declaration.resource_async(
                    "pending",
                    1,
                    &[],
                    move |_| {
                        async move {
                            opened.set(true);
                            settled.await.unwrap();
                            Ok(())
                        }
                        .boxed_local()
                    },
                    move |_, _| {
                        async move {
                            closes.set(closes.get() + 1);
                            Ok(())
                        }
                        .boxed_local()
                    },
                )
            }
        },
    ))])
    .unwrap();
    let (connection, frames) = connection(&host, options());
    get(&connection, 1, "pending");
    pool.run_until_stalled();
    assert!(opened.get());
    assert!(frames.borrow().is_empty());
    connection
        .receive(br#"{"op":"cancel","payload":{"id":1}}"#)
        .unwrap();
    pool.run_until_stalled();
    assert_eq!(closes.get(), 0);
    assert_eq!(host.kernel().diagnostics().objects, 1);
    settle.send(()).unwrap();
    pool.run_until_stalled();
    assert_eq!(response(&frames, 1)["error"]["code"], "cancelled");
    assert_eq!(closes.get(), 1);
    assert_eq!(host.kernel().diagnostics().objects, 0);
    assert!(!connection.is_closed());
    connection.close();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn cancellation_and_disconnect_keep_resources_until_the_callback_physically_settles() {
    for disconnect in [false, true] {
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        let (settle, settled) = oneshot::channel();
        let settled = Rc::new(RefCell::new(Some(settled)));
        let entered = Rc::new(Cell::new(false));
        let cancelled = Rc::new(Cell::new(false));
        let closes = Rc::new(Cell::new(0));
        host.install(vec![
            Orb::new("pending", 1).service(
                Service::new("pending", {
                    let (settled, entered, cancelled, closes) = (
                        settled.clone(),
                        entered.clone(),
                        cancelled.clone(),
                        closes.clone(),
                    );
                    move |declaration| {
                        let closes = closes.clone();
                        declaration.resource(
                            "owned",
                            1,
                            || Ok(()),
                            move |_, _| {
                                let closes = closes.clone();
                                async move {
                                    closes.set(closes.get() + 1);
                                    Ok(())
                                }
                                .boxed_local()
                            },
                        )?;
                        Ok((settled.clone(), entered.clone(), cancelled.clone()))
                    }
                })
                .method("wait", |environment, ctx, (): ()| {
                    let settled = environment.0.borrow_mut().take().unwrap();
                    async move {
                        environment.1.set(true);
                        ctx.signal().cancelled().await;
                        environment.2.set(true);
                        settled.await.unwrap();
                        ctx.check()?;
                        Ok(())
                    }
                }),
            ),
        ])
        .unwrap();
        let (connection, frames) = connection(&host, options());
        get(&connection, 1, "pending");
        pool.run_until_stalled();
        let reference = response(&frames, 1)["ok"]["reference"].as_u64().unwrap();
        call(&connection, 2, reference, "wait", json!([]));
        pool.run_until_stalled();
        assert!(entered.get());
        if disconnect {
            connection.close();
        } else {
            connection
                .receive(br#"{"op":"cancel","payload":{"id":2}}"#)
                .unwrap();
            request(&connection, 3, "release", json!({"reference": reference}));
        }
        pool.run_until_stalled();
        assert!(cancelled.get());
        assert_eq!(closes.get(), 0);
        assert_eq!(host.kernel().diagnostics().objects, 1);
        settle.send(()).unwrap();
        pool.run_until_stalled();
        assert_eq!(closes.get(), 1);
        assert_eq!(host.kernel().diagnostics().objects, 0);
        assert_eq!(host.kernel().diagnostics().calls, 0);
        assert_eq!(host.kernel().diagnostics().roots, 0);
        if !disconnect {
            assert_eq!(response(&frames, 2)["error"]["code"], "cancelled");
        }
        connection.close();
        pool.run_until(host.close()).unwrap();
    }
}

#[derive(Clone, Copy)]
enum SharedStateFailure {
    Sender,
    FrameLimit,
}

fn assert_shared_state_failure_isolated(failure: SharedStateFailure) {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let (settle, settled) = oneshot::channel();
    let settled = Rc::new(RefCell::new(Some(settled)));
    let entered = Rc::new(Cell::new(false));
    let cancelled = Rc::new(Cell::new(false));
    let closes = Rc::new(Cell::new(0));
    host.install(vec![
        Orb::new("example", 1)
            .service(
                Service::new("shared", |declaration| {
                    declaration.state("initial".to_owned(), 1, |_| Ok(()))
                })
                .method("set", |state, ctx, (value,): (String,)| async move {
                    let length = value.len();
                    state.update(&ctx, |_| Ok(value))?;
                    Ok(length)
                }),
            )
            .service(
                Service::new("pending", {
                    let (entered, cancelled, closes) =
                        (entered.clone(), cancelled.clone(), closes.clone());
                    move |declaration| {
                        let closes = closes.clone();
                        declaration.resource(
                            "owned",
                            1,
                            || Ok(()),
                            move |_, _| {
                                async move {
                                    closes.set(closes.get() + 1);
                                    Ok(())
                                }
                                .boxed_local()
                            },
                        )?;
                        Ok((settled.clone(), entered.clone(), cancelled.clone()))
                    }
                })
                .method("wait", |environment, ctx, (): ()| {
                    let settled = environment.0.borrow_mut().take().unwrap();
                    async move {
                        environment.1.set(true);
                        ctx.signal().cancelled().await;
                        environment.2.set(true);
                        settled.await.unwrap();
                        ctx.check()
                    }
                }),
            ),
    ])
    .unwrap();

    let mut broken_options = options();
    broken_options.max_frame = match failure {
        SharedStateFailure::Sender => 4096,
        SharedStateFailure::FrameLimit => 256,
    };
    let broken_frames = Rc::new(RefCell::new(Vec::<Value>::new()));
    let sent = broken_frames.clone();
    let state_attempts = Rc::new(Cell::new(0));
    let attempted = state_attempts.clone();
    let broken = serve(host.clone(), broken_options, move |bytes| {
        let frame: Value = serde_json::from_slice(&bytes).unwrap();
        if frame["event"] == "state" {
            attempted.set(attempted.get() + 1);
            if matches!(failure, SharedStateFailure::Sender) {
                return Err(Error::new(
                    Code::Unavailable,
                    "observer_transport",
                    "state delivery stopped",
                ));
            }
        }
        sent.borrow_mut().push(frame);
        Ok(())
    });
    get(&broken, 1, "shared");
    pool.run_until_stalled();
    assert_eq!(response(&broken_frames, 1)["ok"]["state"], "initial");
    get(&broken, 2, "pending");
    pool.run_until_stalled();
    let pending_reference = response(&broken_frames, 2)["ok"]["reference"]
        .as_u64()
        .unwrap();
    call(&broken, 3, pending_reference, "wait", json!([]));
    pool.run_until_stalled();
    assert!(entered.get());

    // The failing observer attaches first so its cleanup runs before the healthy observer.
    let mut healthy_options = options();
    healthy_options.max_frame = 4096;
    let (healthy, healthy_frames) = connection(&host, healthy_options);
    get(&healthy, 1, "shared");
    pool.run_until_stalled();
    assert_eq!(host.kernel().diagnostics().objects, 2);
    let shared_reference = response(&healthy_frames, 1)["ok"]["reference"]
        .as_u64()
        .unwrap();
    let updated = match failure {
        SharedStateFailure::Sender => "first update".to_owned(),
        SharedStateFailure::FrameLimit => "x".repeat(1024),
    };
    call(&healthy, 2, shared_reference, "set", json!([updated]));
    pool.run_until_stalled();
    assert_eq!(response(&healthy_frames, 2)["ok"], updated.len());
    assert!(!healthy.is_closed());
    assert!(broken.is_closed());
    assert!(cancelled.get());
    assert_eq!(closes.get(), 0);
    assert_eq!(host.kernel().diagnostics().objects, 2);
    let cause = broken.close_cause().unwrap();
    match failure {
        SharedStateFailure::Sender => {
            assert_eq!(cause.code, Code::Unavailable);
            assert_eq!(cause.reason, "observer_transport");
            assert_eq!(state_attempts.get(), 1);
        }
        SharedStateFailure::FrameLimit => {
            assert_eq!(cause.code, Code::Exhausted);
            assert_eq!(cause.reason, "frame_size");
            assert_eq!(state_attempts.get(), 0);
        }
    }
    call(&healthy, 3, shared_reference, "set", json!(["next update"]));
    pool.run_until_stalled();
    assert_eq!(response(&healthy_frames, 3)["ok"], "next update".len());
    let states: Vec<_> = healthy_frames
        .borrow()
        .iter()
        .filter(|frame| frame["event"] == "state")
        .cloned()
        .collect();
    assert_eq!(
        states,
        vec![
            json!({"event": "state", "payload": {"reference": shared_reference, "value": updated}}),
            json!({"event": "state", "payload": {"reference": shared_reference, "value": "next update"}}),
        ]
    );
    assert_eq!(broken_frames.borrow().len(), 2);
    assert!(healthy.close_cause().is_none());
    settle.send(()).unwrap();
    pool.run_until_stalled();
    assert_eq!(closes.get(), 1);
    assert_eq!(host.kernel().diagnostics().objects, 1);
    assert_eq!(host.kernel().diagnostics().references, 1);
    assert_eq!(host.kernel().diagnostics().calls, 0);
    assert_eq!(host.kernel().diagnostics().roots, 0);
    assert_eq!(host.kernel().diagnostics().resources, 0);
    healthy.close();
    pool.run_until(host.close()).unwrap();
    let diagnostics = host.kernel().diagnostics();
    assert_eq!(diagnostics.objects, 0);
    assert_eq!(diagnostics.references, 0);
    assert_eq!(diagnostics.calls, 0);
    assert_eq!(diagnostics.roots, 0);
    assert_eq!(diagnostics.resources, 0);
    assert_eq!(diagnostics.tasks, 0);
    assert!(host.errors().is_empty());
}

#[test]
fn shared_state_sender_failure_isolates_the_other_connection_and_waits_for_cleanup() {
    assert_shared_state_failure_isolated(SharedStateFailure::Sender);
}

#[test]
fn shared_state_frame_limit_isolates_the_other_connection_and_waits_for_cleanup() {
    assert_shared_state_failure_isolated(SharedStateFailure::FrameLimit);
}
