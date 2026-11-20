//! Checked delivery retains resource claims until acceptance and rejects reused request IDs.
use super::*;
use tembra::{
    contracts::Contract,
    portable::{Issuable, Nominal, Payload},
};

const CODEC: &str = "example:rpc-check/codec@1.0.0";
const VALUES: &str = "example:rpc-check/values@1.0.0";

struct Handle(Rc<Cell<u32>>);

impl Issuable for Handle {
    fn nominal() -> Nominal {
        Nominal {
            interface: CODEC.into(),
            name: "handle".into(),
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

/// Serve actual canonical data and resource methods through the existing portable boundary.
fn provider(closed: Rc<Cell<u32>>) -> Orb {
    let contract = Contract::from_source(
        "rpc.wit",
        r#"
        package example:rpc-check@1.0.0;
        interface values { echo: func(value: u32) -> u32; }
        interface codec { resource handle { read: func() -> u32; } open: func() -> handle; }
        world provider { export values; export codec; }
    "#,
        Some("provider"),
    )
    .unwrap();
    let prepared = contract.prepared().encode();
    let signatures = contract.prepared().service_signatures().unwrap();
    Orb::new("checked", 1)
        .service(
            Service::new("values", |_| Ok(()))
                .signature(Some(signatures[VALUES].clone()))
                .contract(VALUES, &prepared)
                .method("echo", |_, _, (value,): (u32,)| async move { Ok(value) }),
        )
        .service(
            Service::new("codec", |_| Ok(()))
                .signature(Some(signatures[CODEC].clone()))
                .contract(CODEC, &prepared)
                .payload_method("open", move |_, _, payload| {
                    let closed = closed.clone();
                    async move {
                        payload.into_data()?;
                        let mut result = Payload::data(Value::Null);
                        let handle = result.issue(Handle(closed));
                        result.encode(handle)
                    }
                })
                .payload_method("[method]handle.read", |_, _, mut payload| async move {
                    let handle = payload.receiver::<Handle>()?;
                    payload.into_data()?;
                    handle.with(|_| Payload::data(json!(7)))
                }),
        )
}

/// A response containing an owner still reserves its request ID after execution settles.
#[test]
fn a_duplicate_id_cannot_replace_an_unaccepted_delivery() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let closed = Rc::new(Cell::new(0));
    host.install(vec![provider(closed.clone())]).unwrap();
    let (connection, frames) = connection(&host, options());
    request(
        &connection,
        1,
        "get",
        json!({"service":"codec", "key":["main"], "resources":1}),
    );
    pool.run_until_stalled();
    let acquired = response(&frames, 1)["ok"].clone();
    request(
        &connection,
        2,
        "call",
        json!({"reference": acquired["reference"], "method":"open", "input":"[]", "capabilities":{"signature":acquired["signature"]}}),
    );
    pool.run_until_stalled();
    assert_eq!(
        response(&frames, 2)["ok"]["owned"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(host.kernel().diagnostics().calls, 0);
    assert_eq!(closed.get(), 0);
    let error = connection
        .receive(br#"{"id":2,"op":"describe","payload":{}}"#)
        .unwrap_err();
    assert!(error.message.contains("duplicate request id"), "{error}");
    assert!(connection.is_closed());
    pool.run_until_stalled();
    assert_eq!(closed.get(), 1);
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().references, 0);
}

/// Invalid cached metadata refuses acquisition before consuming a reference identity.
#[test]
fn a_failed_contract_check_does_not_consume_a_reference_id() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        Orb::new("checks", 1)
            .service(
                Service::new("broken", |_| Ok(()))
                    .contract(VALUES, b"invalid")
                    .method("echo", |_, _, (value,): (u32,)| async move { Ok(value) }),
            )
            .service(
                Service::new("valid", |_| Ok(()))
                    .method("echo", |_, _, (value,): (u32,)| async move { Ok(value) }),
            ),
    ])
    .unwrap();
    let (connection, frames) = connection(&host, options());
    get(&connection, 1, "broken");
    pool.run_until_stalled();
    assert_eq!(response(&frames, 1)["error"]["reason"], "contract");
    get(&connection, 2, "valid");
    pool.run_until_stalled();
    assert_eq!(response(&frames, 2)["ok"]["reference"], 1);
    connection.close();
    pool.run_until(host.close()).unwrap();
}

/// Compare identical public RPC workloads before and after contract preparation changes.
#[test]
#[ignore = "operational workload comparison: run explicitly with --nocapture"]
fn measure_rpc_contract_preparation() {
    for resource in [false, true] {
        for trial in 0..5 {
            let mut pool = LocalPool::new();
            let host = Host::new(pool.spawner());
            let closed = Rc::new(Cell::new(0));
            host.install(vec![provider(closed)]).unwrap();
            let (connection, frames) = connection(&host, options());
            request(
                &connection,
                1,
                "get",
                json!({"service": if resource {"codec"} else {"values"}, "key":["main"], "resources":1}),
            );
            pool.run_until_stalled();
            let acquired = response(&frames, 1)["ok"].clone();
            let mut descriptor = Value::Null;
            if resource {
                request(
                    &connection,
                    2,
                    "call",
                    json!({"reference":acquired["reference"], "method":"open", "input":"[]", "capabilities":{"signature":acquired["signature"]}}),
                );
                pool.run_until_stalled();
                descriptor = response(&frames, 2)["ok"]["owned"][0].clone();
                connection
                    .receive(br#"{"op":"accept","payload":{"id":2}}"#)
                    .unwrap();
            }
            frames.borrow_mut().clear();
            let started = std::time::Instant::now();
            for id in 3..1003 {
                let payload = if resource {
                    json!({"reference":descriptor["reference"], "method":"[method]handle.read", "input":"[]", "capabilities":{"signature":acquired["signature"], "receiver":descriptor}})
                } else {
                    json!({"reference":acquired["reference"], "method":"echo", "input":"[7]", "capabilities":{"signature":acquired["signature"]}})
                };
                request(&connection, id, "call", payload);
                pool.run_until_stalled();
                assert_eq!(response(&frames, id)["ok"]["output"], "7");
                frames.borrow_mut().clear();
            }
            println!(
                "{}",
                json!({"resource":resource, "trial":trial, "calls":1000, "elapsed_us":started.elapsed().as_micros()})
            );
            connection.close();
            pool.run_until(host.close()).unwrap();
        }
    }
}
