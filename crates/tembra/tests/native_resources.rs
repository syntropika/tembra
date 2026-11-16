//! WIT resources issued by a native exporter through the typed portable API.
//!
//! The exporter issues plain Rust values as capabilities of a WIT resource type with
//! `Payload::issue`, places their `Slot`s in its results wherever compiled exporters return an
//! owned handle, and reads a method's implicit `self` back as the value with `Payload::receiver`.
//! Native callers hold the results as `Owned` handles or as a `Delivered` batch. No executor,
//! Wasm or contract is involved.
use futures::{FutureExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tembra::{
    Host, Orb, Reference, Service, core,
    portable::{Capability, Delivered, Issuable, Issued, Nominal, Owned, Payload, Slot},
};

const INTERFACE: &str = "test:media/codecs@1.0.0";
const CONSTRUCTOR: &str = "[constructor]decoder";
const FEED: &str = "[method]decoder.feed";
const OPEN_BUNDLE: &str = "open-bundle";
const BUFFER: &str = "[constructor]buffer";
const FOREIGN: &str = "foreign";

/// The exporter's own value. Dropping it is the close.
struct Decoder {
    id: u32,
    closed: Rc<Cell<u32>>,
}

impl Issuable for Decoder {
    fn nominal() -> Nominal {
        Nominal {
            interface: INTERFACE.into(),
            name: "decoder".into(),
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.closed.set(self.closed.get() + 1);
    }
}

/// Another resource type of the same interface.
struct Buffer;

impl Issuable for Buffer {
    fn nominal() -> Nominal {
        Nominal {
            interface: INTERFACE.into(),
            name: "buffer".into(),
        }
    }
}

/// A decoder of another issuer: a hand-written backend with the decoder's resource type.
struct Foreign(Nominal);

impl Capability for Foreign {
    fn nominal(&self) -> &Nominal {
        &self.0
    }
}

/// The result of `open-bundle`: a WIT `result<bundle, string>`.
#[derive(Serialize)]
#[serde(tag = "tag", content = "val", rename_all = "kebab-case")]
enum Opened {
    Ok(Bundle),
    Err(String),
}

/// Owned decoders in every nested position compiled exporters support.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Bundle {
    primary: Slot,
    optional: Option<Slot>,
    extras: Vec<Slot>,
    pair: (Slot, u32),
    choice: Choice,
    count: u32,
}

#[derive(Serialize)]
#[serde(tag = "tag", content = "val", rename_all = "kebab-case")]
enum Choice {
    Decoder(Slot),
}

/// Provider state shared with the test.
#[derive(Default)]
struct Codecs {
    closed: Rc<Cell<u32>>,
    next: Cell<u32>,
    feed_gate: RefCell<Option<oneshot::Receiver<()>>>,
    feed_entered: Cell<bool>,
}

impl Codecs {
    fn decoder(&self) -> Decoder {
        let id = self.next.get() + 1;
        self.next.set(id);
        Decoder {
            id,
            closed: self.closed.clone(),
        }
    }
}

fn arguments<A: DeserializeOwned>(payload: Payload) -> tembra::Result<A> {
    serde_json::from_value(payload.into_data()?)
        .map_err(|error| tembra::Error::invalid(core::Reason::Arguments, error.to_string()))
}

fn service(codecs: Rc<Codecs>) -> Service<Rc<Codecs>> {
    Service::new("codecs", move |_| Ok(codecs.clone()))
        .payload_method(CONSTRUCTOR, |codecs, _, payload: Payload| async move {
            payload.into_data()?;
            let mut result = Payload::data(Value::Null);
            let decoder = result.issue(codecs.decoder());
            result.encode(decoder)
        })
        .payload_method(FEED, |codecs, _, mut payload: Payload| {
            let gate = codecs.feed_gate.borrow_mut().take();
            async move {
                let decoder = payload.receiver::<Decoder>()?;
                let (n,): (u32,) = arguments(payload)?;
                codecs.feed_entered.set(true);
                if let Some(gate) = gate {
                    let _ = gate.await;
                }
                // The value is still the method's after the await.
                Ok(Payload::data(json!(decoder.id * 1000 + n)))
            }
        })
        .payload_method(OPEN_BUNDLE, |codecs, _, payload: Payload| async move {
            let (fail,): (bool,) = arguments(payload)?;
            let mut result = Payload::data(Value::Null);
            let opened = if fail {
                Opened::Err("refused".into())
            } else {
                Opened::Ok(Bundle {
                    primary: result.issue(codecs.decoder()),
                    optional: Some(result.issue(codecs.decoder())),
                    extras: vec![
                        result.issue(codecs.decoder()),
                        result.issue(codecs.decoder()),
                    ],
                    pair: (result.issue(codecs.decoder()), 7),
                    choice: Choice::Decoder(result.issue(codecs.decoder())),
                    count: 6,
                })
            };
            result.encode(opened)
        })
        .payload_method(BUFFER, |_, _, payload: Payload| async move {
            payload.into_data()?;
            let mut result = Payload::data(Value::Null);
            let buffer = result.issue(Buffer);
            result.encode(buffer)
        })
        .payload_method(FOREIGN, |_, _, payload: Payload| async move {
            payload.into_data()?;
            let mut result = Payload::data(Value::Null);
            let index = result.attach(Issued::new(Foreign(Decoder::nominal())));
            result.map_value(|_| Ok(json!(index)))
        })
}

struct Fixture {
    pool: LocalPool,
    host: Host,
    codecs: Rc<Codecs>,
}

fn fixture() -> Fixture {
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let codecs = Rc::new(Codecs::default());
    host.install(vec![Orb::new("media", 1).service(service(codecs.clone()))])
        .unwrap();
    Fixture { pool, host, codecs }
}

impl Fixture {
    fn reference(&self) -> Rc<Reference> {
        Rc::new(
            self.host
                .get("codecs", vec!["main".into()], "host")
                .unwrap(),
        )
    }

    fn root<T: 'static>(
        &mut self,
        operation: impl AsyncFnOnce(tembra::Context) -> tembra::Result<T> + 'static,
    ) -> tembra::Result<T> {
        self.pool.run_until(
            self.host
                .root("host", move |context| operation(context).boxed_local()),
        )
    }

    fn construct(&mut self, method: &'static str) -> Owned {
        let called = self.reference();
        self.root(async move |context| called.call_portable(&context, method, ()).await)
            .unwrap()
    }

    fn closed(&self) -> u32 {
        self.codecs.closed.get()
    }

    fn close(mut self) {
        self.pool.run_until(self.host.close()).unwrap();
    }
}

#[test]
fn a_constructor_issues_a_value_that_a_method_reads_as_self_until_its_last_owner_drops() {
    let mut fixture = fixture();
    let owned = Rc::new(fixture.construct(CONSTRUCTOR));
    assert_eq!(owned.nominal(), &Decoder::nominal());
    let (finish, gate) = oneshot::channel();
    *fixture.codecs.feed_gate.borrow_mut() = Some(gate);
    let fed = Rc::new(RefCell::new(None));
    {
        let host = fixture.host.clone();
        let caller = owned.clone();
        let fed = fed.clone();
        fixture
            .pool
            .spawner()
            .spawn_local(async move {
                let result = host
                    .root("host", move |context| {
                        async move {
                            caller
                                .call_portable::<_, u32>(&context, FEED, (5_u32,))
                                .await
                        }
                        .boxed_local()
                    })
                    .await;
                *fed.borrow_mut() = Some(result);
            })
            .unwrap();
    }
    fixture.pool.run_until_stalled();
    assert!(fixture.codecs.feed_entered.get());
    // The test's owner goes while the method still holds the value across its await.
    drop(owned);
    fixture.pool.run_until_stalled();
    assert_eq!(fixture.closed(), 0, "the running method keeps its self");
    finish.send(()).unwrap();
    fixture.pool.run_until_stalled();
    assert_eq!(fed.borrow_mut().take().unwrap().unwrap(), 1005);
    assert_eq!(fixture.closed(), 1, "dropped with its last owner");
    fixture.close();
}

#[test]
fn owned_results_nest_in_every_position_compiled_exporters_support() {
    let mut fixture = fixture();
    let reference = fixture.reference();
    let called = reference.clone();
    let delivered: Delivered = fixture
        .root(async move |context| called.call_portable(&context, OPEN_BUNDLE, (false,)).await)
        .unwrap();
    assert_eq!(
        delivered.value,
        json!({
            "tag": "ok",
            "val": {
                "primary": 0,
                "optional": 1,
                "extras": [2, 3],
                "pair": [4, 7],
                "choice": { "tag": "decoder", "val": 5 },
                "count": 6,
            },
        })
    );
    assert_eq!(delivered.owned.len(), 6);
    assert!(delivered.streams.is_empty());
    // Each slot names its own value, in issue order.
    let fed = fixture
        .root(async move |context| {
            let mut fed = Vec::new();
            for owned in &delivered.owned {
                assert_eq!(owned.nominal(), &Decoder::nominal());
                fed.push(
                    owned
                        .call_portable::<_, u32>(&context, FEED, (1_u32,))
                        .await?,
                );
            }
            Ok((fed, delivered))
        })
        .unwrap();
    assert_eq!(fed.0, [1001, 2001, 3001, 4001, 5001, 6001]);
    assert_eq!(fixture.closed(), 0);
    drop(fed.1);
    assert_eq!(fixture.closed(), 6, "every value dropped with its owner");
    let called = reference.clone();
    let refused: Delivered = fixture
        .root(async move |context| called.call_portable(&context, OPEN_BUNDLE, (true,)).await)
        .unwrap();
    assert_eq!(refused.value, json!({ "tag": "err", "val": "refused" }));
    assert!(refused.owned.is_empty());
    reference.release().unwrap();
    fixture.close();
}

#[test]
fn a_method_requires_a_self_of_its_own_type_and_issuer() {
    let mut fixture = fixture();
    let reference = fixture.reference();
    let called = reference.clone();
    let without_self = fixture
        .root(async move |context| {
            called
                .call_portable::<_, u32>(&context, FEED, (1_u32,))
                .await
        })
        .unwrap_err();
    assert_eq!(without_self.code, core::Code::Invalid, "{without_self}");
    assert_eq!(without_self.reason, "resource");
    let buffer = fixture.construct(BUFFER);
    assert_eq!(buffer.nominal(), &Buffer::nominal());
    let mistyped = fixture
        .root(async move |context| {
            buffer
                .call_portable::<_, u32>(&context, FEED, (1_u32,))
                .await
        })
        .unwrap_err();
    assert_eq!(mistyped.code, core::Code::Forbidden, "{mistyped}");
    assert_eq!(mistyped.reason, "resource");
    let foreign = fixture.construct(FOREIGN);
    assert_eq!(foreign.nominal(), &Decoder::nominal());
    let foreign = fixture
        .root(async move |context| {
            foreign
                .call_portable::<_, u32>(&context, FEED, (1_u32,))
                .await
        })
        .unwrap_err();
    assert_eq!(foreign.code, core::Code::Gone, "{foreign}");
    assert_eq!(foreign.reason, "resource");
    assert_eq!(fixture.closed(), 0, "no decoder was issued");
    reference.release().unwrap();
    fixture.close();
}
