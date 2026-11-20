//! All existing portable containers retain owned resource identity across native imports.
use futures::{FutureExt, executor::LocalPool};
use std::rc::Rc;
use tembra::{Host, Provider};

/// A native transport fixture issues actual capabilities into the fixed contract's containers.
#[test]
fn imported_containers_redeem_resources_without_serializing_their_owners() {
    use serde_json::{Value, json};
    use std::cell::Cell;
    use tembra::{
        Orb, Service,
        portable::{Issuable, Nominal, Payload},
    };
    use tembra_orb_native_fixture::codec::packet_reader;
    struct Ticket(u64, Rc<Cell<u32>>);
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
            self.1.set(self.1.get() + 1);
        }
    }
    let closed = Rc::new(Cell::new(0));
    let seen = closed.clone();
    let interface = "packets::example:protocol/packets@1.0.0";
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
        Service::new(interface, |_| Ok(())).signature(Some(signature))
            .payload_method("receive", move |_, _, payload: Payload| {
                let closed = seen.clone();
                async move {
                    payload.into_data()?;
                    let mut payload = Payload::data(Value::Null);
                    let first: u32 = payload.issue(Ticket(11, closed.clone())).into();
                    let second: u32 = payload.issue(Ticket(13, closed)).into();
                    payload.encode(json!({ "tag": "ok", "val": [[null, {"tag": "ok"}, {"tag": "created", "val": {"ticket": first, "amount": 7}}], second] }))
                }
            })
            .payload_method("[method]ticket.value", |_, _, mut payload: Payload| async move {
                let value = payload.receiver::<Ticket>()?.with(|ticket| ticket.0)?;
                Payload::data(Value::Null).encode(value)
            }),
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
    let result = pool
        .run_until(host.root("host", move |context| {
            async move {
                called
                    .call_portable::<_, u64>(&context, "read", Vec::<u32>::new())
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(result, 31);
    assert_eq!(closed.get(), 2);
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
