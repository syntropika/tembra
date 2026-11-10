//! Host-issued capabilities at the native portable boundary.
//!
//! The provider stands in for a resource-issuing executor: a `payload_method` constructor
//! issues an opaque test capability, and resource methods recover it from their `self` loan.
//! No executor, Wasm or contract is involved. These tests cover ownership, delivery and
//! admission at the native boundary only.
use futures::{
    FutureExt,
    channel::oneshot,
    executor::LocalPool,
    future::{AbortHandle, Abortable},
    task::LocalSpawnExt,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
};
use tembra::{
    AcquisitionOptions, Host, Orb, Reference, Service, core,
    portable::{Capability, Delivered, FromPayload, Issued, Loan, Nominal, Origin, Owned, Payload},
};

const CONSTRUCTOR: &str = "[constructor]decoder";
const FEED: &str = "[method]decoder.feed";
const SECRET: &str = "[method]decoder.secret";

fn nominal() -> Nominal {
    Nominal {
        interface: "test:media/codecs@1.0.0".into(),
        name: "decoder".into(),
    }
}

/// The issuer's backend: dropping it is the close.
struct Decoder {
    nominal: Nominal,
    id: u32,
    closed: Rc<Cell<u32>>,
}

impl Capability for Decoder {
    fn nominal(&self) -> &Nominal {
        &self.nominal
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.closed.set(self.closed.get() + 1);
    }
}

/// Provider state shared with the test.
#[derive(Default)]
struct Codecs {
    closed: Rc<Cell<u32>>,
    entered: Cell<bool>,
    next: Cell<u32>,
    gate: RefCell<Option<oneshot::Receiver<()>>>,
    retained: RefCell<Option<Loan>>,
    feed_gate: RefCell<Option<oneshot::Receiver<()>>>,
    feed_entered: Cell<bool>,
    payload: RefCell<Option<Payload>>,
}

#[derive(Deserialize)]
struct Input {
    name: String,
    count: u32,
}

/// Any serde use of this type fails the test: it proves the typed local path never encodes.
struct Opaque(u32);

impl Serialize for Opaque {
    fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        panic!("the typed local path serialized a value")
    }
}

impl<'de> Deserialize<'de> for Opaque {
    fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        panic!("the typed local path deserialized a value")
    }
}

fn service(codecs: Rc<Codecs>) -> Service<Rc<Codecs>> {
    let service = Service::new("codecs", move |_| Ok(codecs.clone()))
        .payload_method(
            CONSTRUCTOR,
            |codecs: Rc<Rc<Codecs>>, _, payload: Payload| {
                codecs.entered.set(true);
                let gate = codecs.gate.borrow_mut().take();
                async move {
                    payload.into_data()?;
                    if let Some(gate) = gate {
                        let _ = gate.await;
                    }
                    let id = codecs.next.get() + 1;
                    codecs.next.set(id);
                    let mut result = Payload::data(Value::Null);
                    let index = result.attach(Issued::new(Decoder {
                        nominal: nominal(),
                        id,
                        closed: codecs.closed.clone(),
                    }));
                    result.map_value(|_| Ok(json!(index)))
                }
            },
        )
        .payload_method(FEED, |codecs, _, mut payload: Payload| async move {
            let loan = payload
                .take_receiver()
                .ok_or_else(|| tembra::Error::invalid(core::Reason::Arguments, "no self"))?;
            let id = loan
                .with_backend::<Decoder, _>(|decoder| {
                    assert_eq!(decoder.nominal(), &nominal());
                    decoder.id
                })
                .expect("issuer backend");
            *codecs.retained.borrow_mut() = Some(loan);
            let gate = codecs.feed_gate.borrow_mut().take();
            codecs.feed_entered.set(true);
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            let (n,): (u32,) = serde_json::from_value(payload.into_data()?).unwrap();
            Ok(Payload::data(json!(id * 1000 + n)))
        })
        .payload_method("echo", |codecs, _, payload| async move {
            codecs.entered.set(true);
            Ok(payload)
        })
        .payload_method("retain", |codecs, _, payload| async move {
            *codecs.payload.borrow_mut() = Some(payload);
            Ok(Payload::data(Value::Null))
        })
        .payload_method(SECRET, |_, _, _: Payload| async move {
            Ok(Payload::data(json!(0)))
        })
        .method("scalar", |_, _, (n,): (u32,)| async move { Ok(n + 1) })
        .method("typed", |_, _, (input,): (Input,)| async move {
            Ok(format!("{}:{}", input.name, input.count))
        })
        .method("untouched", |_, _, (value,): (Opaque,)| async move {
            Ok(Opaque(value.0 + 1))
        });
    #[cfg(feature = "views")]
    let service = service.access(|principal| {
        if principal == "viewer" {
            BTreeSet::from([CONSTRUCTOR.to_owned(), FEED.to_owned()])
        } else {
            [
                CONSTRUCTOR,
                FEED,
                SECRET,
                "scalar",
                "typed",
                "untouched",
                "echo",
                "retain",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        }
    });
    service
}

struct Fixture {
    pool: LocalPool,
    host: Host,
    codecs: Rc<Codecs>,
}

fn fixture(isolated: bool) -> Fixture {
    let pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let codecs = Rc::new(Codecs::default());
    let orb = Orb::new("media", 1).service(service(codecs.clone()));
    host.install(vec![if isolated { orb.isolated() } else { orb }])
        .unwrap();
    Fixture { pool, host, codecs }
}

impl Fixture {
    fn reference(&self, principal: &str) -> Rc<Reference> {
        Rc::new(
            self.host
                .get("codecs", vec!["main".into()], principal)
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

    fn references(&self) -> usize {
        self.host.kernel().diagnostics().references
    }

    fn calls(&self) -> usize {
        self.host.kernel().diagnostics().calls
    }

    fn close(mut self) {
        self.pool.run_until(self.host.close()).unwrap();
    }
}

thread_local! {
    static OBSERVED: RefCell<Option<Host>> = const { RefCell::new(None) };
    static CALLS_AT_DECODE: Cell<Option<usize>> = const { Cell::new(None) };
}

/// A downstream result type that records the live kernel calls while it decodes.
struct Probe(#[allow(dead_code)] Owned);

impl FromPayload for Probe {
    fn from_payload(payload: Payload, origin: &Origin) -> tembra::Result<Self> {
        let calls = OBSERVED.with(|host| {
            host.borrow()
                .as_ref()
                .map(|host| host.kernel().diagnostics().calls)
        });
        CALLS_AT_DECODE.set(calls);
        Owned::from_payload(payload, origin).map(Probe)
    }
}

#[test]
fn a_lost_caller_closes_the_decoded_capability_once_after_decoding_inside_the_call() {
    let mut fixture = fixture(true);
    OBSERVED.with(|slot| *slot.borrow_mut() = Some(fixture.host.clone()));
    let (open, gate) = oneshot::channel();
    *fixture.codecs.gate.borrow_mut() = Some(gate);
    let reference = fixture.reference("host");
    let references = fixture.references();
    let (abort, registration) = AbortHandle::new_pair();
    let caller = {
        let host = fixture.host.clone();
        let reference = reference.clone();
        Abortable::new(
            async move {
                host.root("host", move |context| {
                    async move {
                        reference
                            .call_portable::<_, Probe>(&context, CONSTRUCTOR, ())
                            .await
                            .map(drop)
                    }
                    .boxed_local()
                })
                .await
            },
            registration,
        )
    };
    fixture
        .pool
        .spawner()
        .spawn_local(caller.map(drop))
        .unwrap();
    fixture.pool.run_until_stalled();
    assert!(fixture.codecs.entered.get());
    assert_eq!(fixture.calls(), 1);
    abort.abort();
    fixture.pool.run_until_stalled();
    open.send(()).unwrap();
    fixture.pool.run_until_stalled();
    assert_eq!(CALLS_AT_DECODE.get(), Some(1), "decoded while admitted");
    assert_eq!(fixture.codecs.closed.get(), 1, "closed exactly once");
    assert_eq!(fixture.references(), references, "anchor released");
    assert_eq!(fixture.calls(), 0);
    OBSERVED.with(|slot| *slot.borrow_mut() = None);
    reference.release().unwrap();
    fixture.close();
}

#[test]
fn data_only_results_refuse_and_close_capabilities() {
    let mut fixture = fixture(true);
    let reference = fixture.reference("host");
    let references = fixture.references();
    let called = reference.clone();
    let error = fixture
        .root(async move |context| {
            called
                .call_portable::<_, u32>(&context, CONSTRUCTOR, ())
                .await
        })
        .unwrap_err();
    assert_eq!(error.reason, "local_value", "{error}");
    assert_eq!(fixture.codecs.closed.get(), 1);
    assert_eq!(fixture.references(), references);
    // A delivered capability that its executor caller drops is closed as well.
    let called = reference.clone();
    let delivered = fixture
        .root(async move |context| {
            called
                .call_portable::<_, Delivered>(&context, CONSTRUCTOR, Payload::data(Value::Null))
                .await
        })
        .unwrap();
    assert_eq!(delivered.owned.len(), 1);
    assert_eq!(
        fixture.references(),
        references + 1,
        "one anchor per capability"
    );
    drop(delivered);
    assert_eq!(fixture.codecs.closed.get(), 2);
    assert_eq!(fixture.references(), references);
    reference.release().unwrap();
    fixture.close();
}

#[test]
fn an_owned_handle_outlives_its_constructor_reference_and_root() {
    let mut fixture = fixture(true);
    let references = fixture.references();
    let (owned, constructor) = fixture
        .root(async move |context| {
            // A root-scoped claim, revoked when this root ends.
            let reference =
                context.get("codecs", vec!["main".into()], AcquisitionOptions::default())?;
            let owned = reference
                .call_portable::<_, Owned>(&context, CONSTRUCTOR, ())
                .await?;
            Ok((owned, reference))
        })
        .unwrap();
    assert_eq!(constructor.status().unwrap(), core::Status::Released);
    assert_eq!(owned.nominal(), &nominal());
    let fed = fixture
        .root(async move |context| {
            let fed = owned
                .call_portable::<_, u32>(&context, FEED, (5_u32,))
                .await?;
            Ok((fed, owned))
        })
        .unwrap();
    assert_eq!(fed.0, 1005);
    assert_eq!(fixture.codecs.closed.get(), 0);
    drop(fed.1);
    assert_eq!(fixture.codecs.closed.get(), 1);
    assert_eq!(fixture.references(), references);
    fixture.close();
}

#[test]
fn an_owned_handle_keeps_the_constructor_reference_restrictions() {
    let mut fixture = fixture(true);
    let reference = Rc::new(
        fixture
            .host
            .get_with_options(
                "codecs",
                vec!["main".into()],
                "host",
                AcquisitionOptions {
                    api_version: 0,
                    methods: BTreeSet::from([CONSTRUCTOR.to_owned(), FEED.to_owned()]),
                },
            )
            .unwrap(),
    );
    let called = reference.clone();
    let (fed, secret) = fixture
        .root(async move |context| {
            let owned = called
                .call_portable::<_, Owned>(&context, CONSTRUCTOR, ())
                .await?;
            // The original claim goes; the handle keeps exactly its admission.
            called.release()?;
            let fed = owned
                .call_portable::<_, u32>(&context, FEED, (1_u32,))
                .await?;
            let secret = owned
                .call_portable::<_, u32>(&context, SECRET, ())
                .await
                .unwrap_err();
            Ok((fed, secret))
        })
        .unwrap();
    assert_eq!(fed, 1001);
    assert_eq!(secret.code, core::Code::Forbidden, "{secret}");
    assert_eq!(fixture.codecs.closed.get(), 1);
    fixture.close();
}

#[cfg(feature = "views")]
#[test]
fn an_owned_handle_keeps_the_constructor_view() {
    let mut fixture = fixture(true);
    let reference = fixture.reference("viewer");
    let called = reference.clone();
    let secret = fixture
        .root(async move |context| {
            let owned = called
                .call_portable::<_, Owned>(&context, CONSTRUCTOR, ())
                .await?;
            called.release()?;
            assert_eq!(
                owned
                    .call_portable::<_, u32>(&context, FEED, (2_u32,))
                    .await?,
                1002
            );
            Ok(owned
                .call_portable::<_, u32>(&context, SECRET, ())
                .await
                .unwrap_err())
        })
        .unwrap();
    assert_eq!(secret.code, core::Code::Forbidden, "{secret}");
    fixture.close();
}

#[test]
fn revoking_the_constructor_reference_before_decoding_closes_the_issued_capability() {
    let mut fixture = fixture(true);
    let (open, gate) = oneshot::channel();
    *fixture.codecs.gate.borrow_mut() = Some(gate);
    let reference = fixture.reference("host");
    let outcome = Rc::new(RefCell::new(None));
    {
        let host = fixture.host.clone();
        let called = reference.clone();
        let outcome = outcome.clone();
        fixture
            .pool
            .spawner()
            .spawn_local(async move {
                let result = host
                    .root("host", move |context| {
                        async move {
                            called
                                .call_portable::<_, Owned>(&context, CONSTRUCTOR, ())
                                .await
                                .map(drop)
                        }
                        .boxed_local()
                    })
                    .await;
                *outcome.borrow_mut() = Some(result);
            })
            .unwrap();
    }
    fixture.pool.run_until_stalled();
    assert!(fixture.codecs.entered.get());
    reference.release().unwrap();
    open.send(()).unwrap();
    fixture.pool.run_until_stalled();
    let result = outcome.borrow_mut().take().expect("call settled");
    assert!(result.is_err(), "no widened or pending grant");
    assert_eq!(fixture.codecs.closed.get(), 1);
    assert_eq!(fixture.references(), 0);
    assert_eq!(fixture.calls(), 0);
    fixture.close();
}

#[test]
fn data_calls_allocate_no_claim_and_typed_local_calls_never_serialize() {
    let mut fixture = fixture(false);
    let reference = fixture.reference("host");
    let references = fixture.references();
    let called = reference.clone();
    let (scalar, delivered, opaque) = fixture
        .root(async move |context| {
            let scalar = called
                .call_portable::<_, u32>(&context, "scalar", (1_u32,))
                .await?;
            let delivered = called
                .call_portable::<_, Delivered>(&context, "scalar", Payload::data(json!([2])))
                .await?;
            let opaque = called
                .call::<_, Opaque>(&context, "untouched", (Opaque(3),))
                .await?;
            Ok((scalar, delivered, opaque.0))
        })
        .unwrap();
    assert_eq!(scalar, 2);
    assert_eq!(delivered.value, json!(3));
    assert!(delivered.owned.is_empty());
    assert_eq!(opaque, 4);
    assert_eq!(fixture.references(), references, "no anchor for data");
    reference.release().unwrap();
    fixture.close();
}

#[test]
fn payload_callers_reach_a_typed_native_provider_through_its_portable_entrypoint() {
    // A dynamic (source or compiled) caller has only an encoded payload. Even for a
    // non-isolated native provider, it must never take the typed local downcast path.
    let mut fixture = fixture(false);
    let reference = fixture.reference("host");
    let called = reference.clone();
    let (typed, plain) = fixture
        .root(async move |context| {
            let typed = called
                .call_portable::<_, Delivered>(
                    &context,
                    "typed",
                    Payload::data(json!([{ "name": "frame", "count": 3 }])),
                )
                .await?;
            let plain = called
                .call_portable::<_, String>(
                    &context,
                    "typed",
                    (json!({ "name": "x", "count": 1 }),),
                )
                .await?;
            Ok((typed.value, plain))
        })
        .unwrap();
    assert_eq!(typed, json!("frame:3"));
    assert_eq!(plain, "x:1");
    reference.release().unwrap();
    fixture.close();
}

#[test]
fn foreign_providers_refuse_capabilities_before_dispatch() {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let invoked = Rc::new(Cell::new(0));
    let seen = invoked.clone();
    let kernel = host.kernel();
    host.set_foreign_dispatcher(move |request| {
        match request["kind"].as_str() {
            // The foreign adapter publishes its own activation.
            Some("activate") => kernel
                .ready(request["acquired"]["object"].as_u64().unwrap())
                .unwrap(),
            Some("invoke") => seen.set(seen.get() + 1),
            _ => {}
        }
        async move { Ok(Value::Null) }.boxed_local()
    });
    let mut orb = core::Orb::new("remote", 1);
    orb.services.push(core::Service::new("remote", ["take"]));
    let reload = host.kernel().prepare(vec![orb], Vec::new()).unwrap();
    host.kernel().commit(reload.id, Vec::new()).unwrap();
    let reference = Rc::new(host.get("remote", vec!["main".into()], "host").unwrap());
    let closed = Rc::new(Cell::new(0));
    let called = reference.clone();
    let capability = closed.clone();
    let error = pool
        .run_until(host.root("host", move |context| {
            async move {
                called.ready().await?;
                let mut payload = Payload::data(Value::Null);
                payload.attach(Issued::new(Decoder {
                    nominal: nominal(),
                    id: 1,
                    closed: capability,
                }));
                called
                    .call_portable::<_, Value>(&context, "take", payload)
                    .await
            }
            .boxed_local()
        }))
        .unwrap_err();
    assert_eq!(error.reason, "local_value", "{error}");
    assert_eq!(closed.get(), 1);
    assert_eq!(invoked.get(), 0, "nothing was dispatched");
    reference.release().unwrap();
    pool.run_until(host.close()).unwrap();
}

#[test]
fn retaining_self_cannot_extend_ownership_but_an_active_call_pins_it() {
    let mut fixture = fixture(true);
    let reference = fixture.reference("host");
    let called = reference.clone();
    let owned: Owned = fixture
        .root(async move |context| called.call_portable(&context, CONSTRUCTOR, ()).await)
        .unwrap();
    let owned = Rc::new(owned);
    let (finish, gate) = oneshot::channel();
    *fixture.codecs.feed_gate.borrow_mut() = Some(gate);
    let host = fixture.host.clone();
    let caller = owned.clone();
    let (abort, registration) = AbortHandle::new_pair();
    fixture
        .pool
        .spawner()
        .spawn_local(async move {
            let result = Abortable::new(
                host.root("host", move |context| {
                    async move {
                        caller
                            .call_portable::<_, u32>(&context, FEED, (7_u32,))
                            .await
                    }
                    .boxed_local()
                }),
                registration,
            )
            .await;
            assert!(
                result.is_err(),
                "caller must be abandoned while the provider runs"
            );
        })
        .unwrap();
    fixture.pool.run_until_stalled();
    assert!(fixture.codecs.feed_entered.get());
    drop(owned);
    abort.abort();
    fixture.pool.run_until_stalled();
    assert_eq!(
        fixture.codecs.closed.get(),
        0,
        "physical borrow is still running"
    );
    assert_eq!(fixture.calls(), 1);
    assert!(
        fixture
            .codecs
            .retained
            .borrow()
            .as_ref()
            .unwrap()
            .with_backend::<Decoder, _>(|decoder| decoder.id)
            .is_some()
    );
    finish.send(()).unwrap();
    fixture.pool.run_until_stalled();
    assert_eq!(fixture.calls(), 0);
    assert_eq!(
        fixture.codecs.closed.get(),
        1,
        "retained weak self does not own the resource"
    );
    assert!(
        fixture
            .codecs
            .retained
            .borrow()
            .as_ref()
            .unwrap()
            .with_backend::<Decoder, _>(|decoder| decoder.id)
            .is_none()
    );
    reference.release().unwrap();
    fixture.close();
}

#[test]
fn owned_arguments_are_refused_before_a_provider_can_echo_them() {
    let mut fixture = fixture(true);
    let reference = fixture.reference("host");
    let called = reference.clone();
    let closed = fixture.codecs.closed.clone();
    let error = fixture
        .root(async move |context| {
            let mut payload = Payload::data(json!(0));
            payload.attach(Issued::new(Decoder {
                nominal: nominal(),
                id: 99,
                closed,
            }));
            called
                .call_portable::<_, Owned>(&context, "echo", payload)
                .await
        })
        .err()
        .expect("an argument cannot mint an owned result on a provider's claim");
    assert_eq!(error.reason, "arguments");
    assert!(!fixture.codecs.entered.get(), "provider was not dispatched");
    assert_eq!(fixture.codecs.closed.get(), 1);
    assert_eq!(fixture.references(), 1, "no anchor was minted");
    assert_eq!(fixture.calls(), 0);
    reference.release().unwrap();
    fixture.close();
}

#[test]
fn a_retained_payload_cannot_forward_its_borrowed_receiver() {
    let mut fixture = fixture(true);
    let reference = fixture.reference("host");
    let called = reference.clone();
    let owned: Owned = fixture
        .root(async move |context| called.call_portable(&context, CONSTRUCTOR, ()).await)
        .unwrap();
    let owned = Rc::new(owned);
    let borrower = owned.clone();
    fixture
        .root(async move |context| {
            borrower
                .call_portable::<_, ()>(&context, "retain", ())
                .await
        })
        .unwrap();
    let retained = fixture.codecs.payload.borrow_mut().take().unwrap();
    fixture.codecs.entered.set(false);
    let called = reference.clone();
    let error = fixture
        .root(async move |context| {
            called
                .call_portable::<_, Value>(&context, "echo", retained)
                .await
        })
        .unwrap_err();
    assert_eq!(error.reason, "arguments");
    assert!(error.message.contains("cannot be forwarded"));
    assert!(
        !fixture.codecs.entered.get(),
        "the retained receiver was not forwarded"
    );
    assert_eq!(fixture.codecs.closed.get(), 0);
    drop(owned);
    assert_eq!(fixture.codecs.closed.get(), 1);
    reference.release().unwrap();
    fixture.close();
}
