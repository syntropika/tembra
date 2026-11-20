//! Resource variant cases retain their WIT spelling in both generated import paths.
use futures::{FutureExt, executor::LocalPool};
use serde_json::{Value, json};
use std::{cell::Cell, rc::Rc};
use tembra::{
    Host, Orb, Provider, Service,
    portable::{Issuable, Nominal, Payload},
};
use tembra_executor_wasm::{Engine, Install, Limits};
use tembra_orb_native_fixture::codec::packet_reader;

const PACKETS: &str = "example:protocol/packets@1.0.0";

struct Ticket(u64, Rc<Cell<u32>>);

impl Issuable for Ticket {
    fn nominal() -> Nominal {
        Nominal {
            interface: PACKETS.into(),
            name: "ticket".into(),
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.1.set(self.1.get() + 1);
    }
}

/// Issue actual owners into the fixed WIT's nested resource containers.
fn provider(case: &'static str, closed: Rc<Cell<u32>>) -> Orb {
    let contract = tembra_contracts::Contract::from_source_in(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../orb-macro/native-fixture/wit"
        ),
        "reader.wit",
        packet_reader::WIT,
        Some("packet-reader"),
    )
    .unwrap();
    let (_, signature, _) = contract.import_requirement(PACKETS, None).unwrap();
    Orb::new("packets", 1).service(
        Service::new(format!("packets::{PACKETS}"), |_| Ok(()))
            .signature(Some(signature))
            .payload_method("receive", move |_, _, payload: Payload| {
                let closed = closed.clone();
                async move {
                    payload.into_data()?;
                    let mut payload = Payload::data(Value::Null);
                    let first: u32 = payload.issue(Ticket(11, closed.clone())).into();
                    let second: u32 = payload.issue(Ticket(13, closed)).into();
                    payload.encode(json!({ "tag": "ok", "val": [[null, {"tag": "ok"}, {"tag": case, "val": {"ticket": first, "amount": 7}}], second] }))
                }
            })
            .payload_method("[method]ticket.value", |_, _, mut payload: Payload| async move {
                let value = payload.receiver::<Ticket>()?.with(|ticket| ticket.0)?;
                Payload::data(Value::Null).encode(value)
            }),
    )
}

/// Ordinary and reserved type names must decode and release owners through either import API.
#[test]
#[ignore = "requires the packet-reader component built by just build-import-resources"]
fn resource_variant_cases_preserve_identity_in_native_and_component_imports() {
    let engine = Engine::new(Limits::default()).unwrap();
    for native in [true, false] {
        for case in ["created", "guest"] {
            let closed = Rc::new(Cell::new(0));
            let reader = if native {
                packet_reader::orb_named(
                    "reader",
                    1,
                    packet_reader::Providers {
                        packets: Provider::new("packets", 1),
                    },
                )
                .unwrap()
            } else {
                super::compiled(
                    &engine,
                    "TEMBRA_WASM_PACKET_READER",
                    "reader",
                    Install::default().requires(PACKETS, "packets", 1),
                )
            };
            let mut pool = LocalPool::new();
            let host = Host::new(pool.spawner());
            host.install(vec![provider(case, closed.clone()), reader])
                .unwrap();
            let reference = Rc::new(
                host.get(
                    "reader::example:packet-reader/reader@1.0.0",
                    vec!["main".into()],
                    "host",
                )
                .unwrap(),
            );
            pool.run_until(reference.ready()).unwrap();
            let claims = host.kernel().diagnostics().references;
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
            assert_eq!(result, 31, "native: {native}, case: {case}");
            assert_eq!(closed.get(), 2, "native: {native}, case: {case}");
            assert_eq!(host.kernel().diagnostics().references, claims);
            reference.release().unwrap();
            pool.run_until(host.close()).unwrap();
            assert!(host.errors().is_empty(), "{:?}", host.errors());
            assert_eq!(engine.instances(), 0);
        }
    }
}
