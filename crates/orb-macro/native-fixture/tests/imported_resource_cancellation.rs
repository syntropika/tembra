//! Cancellation and abandonment preserve the existing portable physical settlement rules.
use futures::{FutureExt, executor::LocalPool};
use std::rc::Rc;
use tembra::{Host, Provider};

/// Abandonment cancels the imported method but retains its physical loan until it settles.
#[test]
fn abandoned_resource_methods_are_joined_before_imported_owners_are_released() {
    exercise(false);
}

#[test]
fn cancelling_an_import_keeps_its_physical_loan_until_settlement() {
    exercise(true);
}

/// Cancel either the imported method alone or its entire calling root.
fn exercise(cancel_root: bool) {
    use futures::channel::oneshot;
    use serde_json::{Value, json};
    use std::cell::{Cell, RefCell};
    use tembra::{
        Context, Orb, Service,
        portable::{Issuable, Nominal, Payload},
    };
    use tembra_orb_native_fixture::codec::packet_reader;
    struct Ticket(Rc<Cell<u32>>);
    impl Issuable for Ticket {
        fn nominal() -> Nominal {
            Nominal {
                interface: "example:protocol/packets@1.0.0".into(),
                name: "ticket".into(),
            }
        }
    }
    impl Drop for Ticket {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let closed = Rc::new(Cell::new(0));
    let issued = closed.clone();
    let cancelled = Rc::new(Cell::new(false));
    let seen = cancelled.clone();
    let (open, gate) = oneshot::channel::<()>();
    let gate = RefCell::new(Some(gate));
    let contract = tembra::contracts::Contract::from_source_in(
        concat!(env!("CARGO_MANIFEST_DIR"), "/wit"),
        "reader.wit",
        packet_reader::WIT,
        Some("packet-reader"),
    )
    .unwrap();
    let (_, signature, _) = contract
        .import_requirement("example:protocol/packets@1.0.0", None)
        .unwrap();
    let provider = Orb::new("packets", 1).service(
        Service::new("packets::example:protocol/packets@1.0.0", |_| Ok(()))
            .signature(Some(signature))
            .payload_method("receive", move |_, _, payload: Payload| {
                let closed = issued.clone();
                async move {
                    payload.into_data()?;
                    let mut payload = Payload::data(Value::Null);
                    let ticket: u32 = payload.issue(Ticket(closed)).into();
                    payload.encode(json!({"tag": "ok", "val": [[], ticket]}))
                }
            })
            .payload_method(
                "[method]ticket.value",
                move |_, context: Context, mut payload: Payload| {
                    let seen = seen.clone();
                    let gate = gate.borrow_mut().take();
                    async move {
                        let receiver = payload.receiver::<Ticket>()?;
                        let _guard = receiver.guard()?;
                        context.signal().cancelled().await;
                        seen.set(true);
                        if let Some(gate) = gate {
                            let _ = gate.await;
                        }
                        Payload::data(Value::Null).encode(7_u64)
                    }
                },
            ),
    );
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![
        provider,
        packet_reader::orb_named(
            "reader",
            1,
            packet_reader::Providers {
                packets: Provider::new("packets", 1),
            },
        )
        .unwrap(),
    ])
    .unwrap();
    let reference = Rc::new(
        host.get(
            "reader::example:packet-reader/reader@1.0.0",
            vec!["main".into()],
            "host",
        )
        .unwrap(),
    );
    let called = reference.clone();
    let root = Rc::new(RefCell::new(None));
    let saved = root.clone();
    let result = host
        .spawn_root("host", move |context| {
            async move {
                saved.replace(Some(context.clone()));
                called
                    .call_portable::<_, u64>(
                        &context,
                        if cancel_root { "read" } else { "glance" },
                        Vec::<u32>::new(),
                    )
                    .await
            }
            .boxed_local()
        })
        .unwrap();
    pool.run_until_stalled();
    if cancel_root {
        root.borrow()
            .as_ref()
            .expect("the root started")
            .cancel()
            .unwrap();
        pool.run_until_stalled();
    }
    assert!(cancelled.get());
    assert_eq!(
        closed.get(),
        0,
        "the import still physically borrows its receiver"
    );
    assert!(host.kernel().diagnostics().calls > 0);
    open.send(()).unwrap();
    let outcome = pool.run_until(result).unwrap();
    // As on Reference, cancellation signals the physical callback; a provider that observes
    // it and then completes successfully can still publish that admitted call's result.
    assert_eq!(outcome.unwrap(), if cancel_root { 7 } else { 0 });
    pool.run_until(host.settled());
    assert_eq!(closed.get(), 1);
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
    assert_eq!(host.kernel().diagnostics().references, 0);
}
