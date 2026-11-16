//! WIT resources issued by a native exporter through the typed portable API.
//!
//! The exporter issues plain Rust values as capabilities of a WIT resource type with
//! `Payload::issue`, places their `Slot`s in its results wherever compiled exporters return an
//! owned handle, and reads a method's implicit `self` back as the value with `Payload::receiver`.
//! Native callers hold the results as `Owned` handles or as a `Delivered` batch. The values
//! belong to the object that issued them, as a compiled exporter's instances belong to their
//! Store: they close with their last owner, and with the object when it is reloaded or released
//! or the host closes. No executor, Wasm or contract is involved.
use futures::{FutureExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    future::poll_fn,
    pin::pin,
    rc::Rc,
    task::Poll,
};
use tembra::{
    Host, Orb, Reference, Service, core,
    portable::{Capability, Delivered, Issuable, Issued, Nominal, Owned, Payload, Receiver, Slot},
};

const INTERFACE: &str = "test:media/codecs@1.0.0";
const CONSTRUCTOR: &str = "[constructor]decoder";
const FEED: &str = "[method]decoder.feed";
const RETAIN: &str = "[method]decoder.retain";
const OPEN_BUNDLE: &str = "open-bundle";
const BUFFER: &str = "[constructor]buffer";
const FOREIGN: &str = "foreign";

/// The exporter's own value. Dropping it is the close, which releases the claim it may hold.
struct Decoder {
    id: u32,
    closed: Rc<Cell<u32>>,
    _held: Option<Rc<Reference>>,
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
    /// A receiver a method kept beyond its call.
    retained: RefCell<Option<Receiver<Decoder>>>,
    /// A claim the next decoder takes with it.
    held: RefCell<Option<Rc<Reference>>>,
    /// Opens and closes of the archive service's resource.
    vault_opened: Rc<Cell<u32>>,
    vault_closed: Rc<Cell<u32>>,
}

impl Codecs {
    fn decoder(&self) -> Decoder {
        let id = self.next.get() + 1;
        self.next.set(id);
        Decoder {
            id,
            closed: self.closed.clone(),
            _held: self.held.borrow_mut().take(),
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
                let id = decoder.with(|decoder| decoder.id)?;
                Ok(Payload::data(json!(id * 1000 + n)))
            }
        })
        .payload_method(RETAIN, |codecs, _, mut payload: Payload| async move {
            let decoder = payload.receiver::<Decoder>()?;
            payload.into_data()?;
            let id = decoder.with(|decoder| decoder.id)?;
            *codecs.retained.borrow_mut() = Some(decoder);
            Ok(Payload::data(json!(id)))
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

/// Another service of the Orb, whose objects own a resource. A reload keeps the resource when
/// its version is unchanged, and closes and reopens it otherwise.
fn archive(opened: Rc<Cell<u32>>, closed: Rc<Cell<u32>>, vault_version: u32) -> Service<()> {
    Service::new("archive", move |declaration| {
        let opened = opened.clone();
        let closed = closed.clone();
        let _vault = declaration.resource(
            "vault",
            vault_version,
            move || {
                opened.set(opened.get() + 1);
                Ok(())
            },
            move |_: Rc<()>, _| {
                closed.set(closed.get() + 1);
                async { Ok(()) }.boxed_local()
            },
        )?;
        Ok(())
    })
}

/// The exporter's Orb: the codecs service and the archive service.
fn media(codecs: Rc<Codecs>, vault_version: u32) -> Orb {
    let vault = (codecs.vault_opened.clone(), codecs.vault_closed.clone());
    Orb::new("media", 1)
        .service(service(codecs))
        .service(archive(vault.0, vault.1, vault_version))
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
    host.install(vec![media(codecs.clone(), 1)]).unwrap();
    Fixture { pool, host, codecs }
}

impl Fixture {
    /// Replace the exporter's Orb and wait for the retired revision's cleanup.
    fn reload(&mut self, orb: Orb) {
        self.pool
            .run_until(self.host.reload(vec![orb], 5000))
            .unwrap();
        self.pool.run_until(self.host.settled());
    }

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

#[test]
fn a_retained_receiver_reads_while_an_owner_lives_and_cannot_extend_the_value() {
    let mut fixture = fixture();
    let owned = fixture.construct(CONSTRUCTOR);
    let (id, owned) = fixture
        .root(async move |context| {
            let id = owned.call_portable::<_, u32>(&context, RETAIN, ()).await?;
            Ok((id, owned))
        })
        .unwrap();
    assert_eq!(id, 1);
    let retained = fixture.codecs.retained.borrow_mut().take().unwrap();
    assert_eq!(
        retained.with(|decoder| decoder.id).unwrap(),
        1,
        "reads while the owner lives"
    );
    drop(owned);
    assert_eq!(
        fixture.closed(),
        1,
        "closed with its last owner, whatever the method kept"
    );
    let gone = retained.with(|decoder| decoder.id).unwrap_err();
    assert_eq!(gone.code, core::Code::Gone, "{gone}");
    assert_eq!(gone.reason, "resource");
    fixture.close();
}

#[test]
fn reloading_the_exporter_closes_its_values_and_retires_their_handles() {
    let mut fixture = fixture();
    let stale = fixture.construct(CONSTRUCTOR);
    let codecs = fixture.codecs.clone();
    fixture.reload(media(codecs, 1));
    assert_eq!(fixture.closed(), 1, "drained with the retired revision");
    let (refused, stale) = fixture
        .root(async move |context| {
            let refused = stale
                .call_portable::<_, u32>(&context, FEED, (1_u32,))
                .await
                .unwrap_err();
            Ok((refused, stale))
        })
        .unwrap();
    assert_eq!(refused.code, core::Code::Gone, "{refused}");
    assert_eq!(refused.reason, "resource");
    // The new revision issues its own values; the stale owner closes nothing twice.
    let fresh = fixture.construct(CONSTRUCTOR);
    let (fed, fresh) = fixture
        .root(async move |context| {
            let fed = fresh
                .call_portable::<_, u32>(&context, FEED, (1_u32,))
                .await?;
            Ok((fed, fresh))
        })
        .unwrap();
    assert_eq!(fed, 2001);
    drop(stale);
    assert_eq!(fixture.closed(), 1);
    drop(fresh);
    assert_eq!(fixture.closed(), 2);
    fixture.close();
}

#[test]
fn a_retired_revision_serves_its_own_roots_until_they_end_and_refuses_newer_ones() {
    let mut fixture = fixture();
    let stale = Rc::new(fixture.construct(CONSTRUCTOR));
    // A root started before the reload keeps the retired revision, and so its values.
    let (finish, gate) = oneshot::channel();
    let caller = stale.clone();
    let old_root = fixture
        .host
        .spawn_root("host", move |context| {
            async move {
                let _ = gate.await;
                caller
                    .call_portable::<_, u32>(&context, FEED, (2_u32,))
                    .await
            }
            .boxed_local()
        })
        .unwrap();
    fixture.pool.run_until_stalled();
    let reloaded = media(fixture.codecs.clone(), 1);
    fixture
        .pool
        .run_until(fixture.host.reload(vec![reloaded], 5000))
        .unwrap();
    fixture.pool.run_until_stalled();
    assert_eq!(fixture.closed(), 0, "kept for the root of its own version");
    // A root of the new revision reaches the new revision, which did not issue the value.
    let caller = stale.clone();
    let refused = fixture
        .root(async move |context| {
            Ok(caller
                .call_portable::<_, u32>(&context, FEED, (1_u32,))
                .await
                .unwrap_err())
        })
        .unwrap();
    assert_eq!(refused.code, core::Code::Gone, "{refused}");
    assert_eq!(refused.reason, "resource");
    // The old root still reads it; its end retires the revision, which closes the value.
    finish.send(()).unwrap();
    assert_eq!(fixture.pool.run_until(old_root).unwrap().unwrap(), 1002);
    fixture.pool.run_until(fixture.host.settled());
    assert_eq!(fixture.closed(), 1, "drained with the retired revision");
    drop(stale);
    assert_eq!(fixture.closed(), 1);
    fixture.close();
}

#[test]
fn a_value_holding_a_claim_on_another_service_closes_at_reload_without_error() {
    let mut fixture = fixture();
    // An archive object exists and the reload changes its vault, so the retired revision cleans
    // up two scopes in one step: the archive's, closing the old vault, and the exporter's,
    // dropping the version with its values. The value's drop reaches the host: it releases
    // the claim it holds, which disposes the archive object.
    let archive = Rc::new(
        fixture
            .host
            .get("archive", vec!["main".into()], "host")
            .unwrap(),
    );
    fixture.pool.run_until(archive.ready()).unwrap();
    let claim = Rc::downgrade(&archive);
    let archive_object = archive.object();
    *fixture.codecs.held.borrow_mut() = Some(archive);
    let owned = fixture.construct(CONSTRUCTOR);
    assert_eq!(fixture.codecs.vault_opened.get(), 1);
    let codecs = fixture.codecs.clone();
    fixture.reload(media(codecs, 2));
    let errors = fixture.host.errors();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(fixture.closed(), 1, "drained with the retired revision");
    assert!(
        claim.upgrade().is_none(),
        "the value released its claim as it closed"
    );
    assert!(
        fixture
            .host
            .kernel()
            .object_address(archive_object)
            .is_err(),
        "the archive object was disposed once unclaimed"
    );
    assert_eq!(
        fixture.codecs.vault_opened.get(),
        2,
        "reopened by the new revision"
    );
    assert_eq!(
        fixture.codecs.vault_closed.get(),
        2,
        "closed with the retired revision, and with the disposed object"
    );
    drop(owned);
    assert_eq!(fixture.closed(), 1);
    fixture.close();
}

#[test]
fn releasing_the_issuing_object_closes_its_values_whatever_claims_remain() {
    let mut fixture = fixture();
    let reference = fixture.reference();
    let owned = fixture.construct(CONSTRUCTOR);
    // Every owner holds a claim on the object, and a reload cannot drop a service with live
    // consumers, so the kernel releases an object under its claims only when it fails it.
    fixture.host.kernel().fail(reference.object()).unwrap();
    fixture.host.flush();
    fixture.pool.run_until(fixture.host.settled());
    assert_eq!(fixture.closed(), 1, "drained with the released object");
    // The kernel refuses the stale handle's call as any call on a released object.
    let (refused, owned) = fixture
        .root(async move |context| {
            let refused = owned
                .call_portable::<_, u32>(&context, FEED, (1_u32,))
                .await
                .unwrap_err();
            Ok((refused, owned))
        })
        .unwrap();
    assert_eq!(refused.code, core::Code::Gone, "{refused}");
    assert_eq!(refused.reason, "object");
    drop(owned);
    assert_eq!(fixture.closed(), 1, "a stale owner closes nothing twice");
    reference.release().unwrap();
    fixture.close();
}

#[test]
fn closing_the_host_closes_the_values_whose_owners_remain() {
    let mut fixture = fixture();
    let reference = fixture.reference();
    let called = reference.clone();
    let delivered: Delivered = fixture
        .root(async move |context| called.call_portable(&context, OPEN_BUNDLE, (false,)).await)
        .unwrap();
    assert_eq!(delivered.owned.len(), 6);
    reference.release().unwrap();
    // A caller the close waits for; it calls once the values have drained.
    let (drained, gate) = oneshot::channel();
    let late = fixture
        .host
        .spawn_root("host", move |context| {
            async move {
                let _ = gate.await;
                let refused = delivered.owned[0]
                    .call_portable::<_, u32>(&context, FEED, (1_u32,))
                    .await
                    .unwrap_err();
                Ok((refused, delivered))
            }
            .boxed_local()
        })
        .unwrap();
    fixture.pool.run_until_stalled();
    let Fixture {
        mut pool,
        host,
        codecs,
    } = fixture;
    pool.run_until(async {
        let mut close = pin!(host.close());
        let drained_first = poll_fn(|context| {
            if codecs.closed.get() == 6 {
                return Poll::Ready(true);
            }
            close.as_mut().poll(context).map(|closed| {
                closed.unwrap();
                false
            })
        })
        .await;
        assert!(drained_first, "the close drains the values before it ends");
        drained.send(()).unwrap();
        close.await.unwrap();
    });
    assert_eq!(codecs.closed.get(), 6, "drained at host close");
    // The close cancelled the caller's root, which refuses its call.
    let (refused, delivered) = pool.run_until(late).unwrap().unwrap();
    assert_eq!(refused.code, core::Code::Cancelled, "{refused}");
    assert_eq!(refused.reason, "root");
    // Owners outliving the host close nothing twice and report no error.
    drop(delivered);
    assert_eq!(codecs.closed.get(), 6);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
}
