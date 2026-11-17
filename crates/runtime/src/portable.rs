//! Host-issued capabilities at the native portable boundary.
//!
//! A portable call moves one [`Payload`]: JSON-shaped data plus host-issued capabilities that
//! JSON cannot express. The issuing executor supplies each capability as an opaque
//! [`Capability`] backend; this crate never names the executor's types. Ownership is plain Rust
//! ownership. Dropping the last holder of an issued capability is its close request, so a
//! result that is refused, undelivered or abandoned closes exactly once.
//!
//! Capabilities never authorize calls by themselves. An [`Owned`] handle calls through its own
//! duplicated kernel claim with the ordinary admission checks. The implicit `self` travels as a
//! [`Loan`] that only the issuer can interpret.
//!
//! A native exporter issues a plain Rust value as a capability of a WIT resource type: the type
//! names its resource type ([`Issuable`]), [`Payload::issue`] issues the value and places its
//! [`Slot`] wherever the result's data names it, and a method reads its implicit `self` back as
//! the value through the [`Receiver`] of [`Payload::receiver`], or holds it across its awaits with
//! the receiver's [`Guard`]. The value belongs to the object version that issued it, as a compiled
//! exporter's instance belongs to its Store: it closes with its last owner, and with that version
//! when the exporter is reloaded or released or the host closes, whatever handles remain, once the
//! calls holding it have ended. A method on such a handle then answers as it would for
//! a compiled exporter: `Gone/resource` from the reloaded exporter, and the kernel's refusal of
//! the call once the object is released or the host has closed.
//!
//! A payload may also carry the end of a byte stream ([`ByteStream`]). The physical work that
//! produces it never travels in a payload: a provider returns it beside its result in an
//! [`Outcome`], and the call's own managed task keeps driving it under the call's invocation.
use crate::{Context, Error, Host, Reference, Result, core, core::Reason};
use futures::{FutureExt, future::LocalBoxFuture};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    marker::PhantomData,
    rc::{Rc, Weak},
};

mod stream;
pub(crate) use stream::RootStream;
pub use stream::{ByteSource, ByteStream};

/// Defining identity of a WIT resource type: its canonical interface and resource name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Nominal {
    pub interface: String,
    pub name: String,
}

/// An issuer's backend for one capability. Its `Drop` is the physical close request.
/// Issuers recover their own type from a [`Loan`] with [`Loan::with_backend`].
pub trait Capability: Any {
    /// The resource type this capability was issued for.
    fn nominal(&self) -> &Nominal;
}

/// A capability freshly issued into a payload. It has one owner, and the issuer keeps no
/// strong reference to it.
pub struct Issued(Rc<dyn Capability>);

impl Issued {
    /// Issue a capability whose backend is now owned only by the payload that carries it.
    pub fn new(capability: impl Capability) -> Self {
        Self(Rc::new(capability))
    }

    pub fn nominal(&self) -> &Nominal {
        self.0.nominal()
    }
}

/// A Rust type whose values a native exporter issues as capabilities of one WIT resource type.
pub trait Issuable: 'static {
    /// The resource type the values are issued as.
    fn nominal() -> Nominal;
}

/// The issuing object version of a native exporter's resources. It owns the values it issued,
/// as a compiled exporter's Store owns its instances; a handle only names an entry. The last
/// owner's drop removes its entry, and the version's end drops the table, which closes every
/// value still owned: the drain compiled exporters do on release and host close.
#[derive(Default)]
pub(crate) struct Issuer {
    values: RefCell<BTreeMap<u64, Rc<dyn Any>>>,
    next: Cell<u64>,
}

impl Issuer {
    fn adopt(&self, value: Rc<dyn Any>) -> u64 {
        let key = self.next.get();
        self.next.set(key + 1);
        self.values.borrow_mut().insert(key, value);
        key
    }

    fn value(&self, key: u64) -> Option<Rc<dyn Any>> {
        self.values.borrow().get(&key).cloned()
    }

    /// Remove a value. It drops outside the table's borrow: its drop may close other handles.
    fn close(&self, key: u64) {
        let value = self.values.borrow_mut().remove(&key);
        drop(value);
    }
}

/// The backend of an issued Rust value. It holds the value until the managed call that returns
/// it stamps the issuing object, so a result refused or undelivered before then closes the value
/// with the backend; afterwards it names the issuer's entry, which its last owner removes. A
/// method's `self` stays pinned by the managed call until that call has physically ended.
struct Backend {
    nominal: Nominal,
    place: RefCell<Place>,
}

enum Place {
    Pending(Rc<dyn Any>),
    Issued { issuer: Weak<Issuer>, key: u64 },
}

impl Backend {
    fn adopt(&self, issuer: &Rc<Issuer>) {
        let mut place = self.place.borrow_mut();
        if let Place::Pending(value) = &*place {
            let key = issuer.adopt(value.clone());
            *place = Place::Issued {
                issuer: Rc::downgrade(issuer),
                key,
            };
        }
    }
}

impl Capability for Backend {
    fn nominal(&self) -> &Nominal {
        &self.nominal
    }
}

impl Drop for Backend {
    /// The last owner's drop closes the value, unless its issuer has drained it already.
    fn drop(&mut self) {
        if let Place::Issued { issuer, key } = self.place.get_mut()
            && let Some(issuer) = issuer.upgrade()
        {
            issuer.close(*key);
        }
    }
}

/// The implicit `self` of one call, read as the value a native exporter issued it from. The
/// call pins the value, so the receiver reads it across awaits within the call. It cannot
/// extend the value's life: retained beyond the call, it reads while an owner lives and answers
/// `Gone/resource` once the last owner or the issuing object has closed the value.
pub struct Receiver<T> {
    issuer: Weak<Issuer>,
    key: u64,
    marker: PhantomData<T>,
}

impl<T: Issuable> Receiver<T> {
    /// Read the value, or refuse as `Gone/resource` once it has closed.
    pub fn with<R>(&self, read: impl FnOnce(&T) -> R) -> Result<R> {
        let value = self
            .issuer
            .upgrade()
            .and_then(|issuer| issuer.value(self.key))
            .ok_or_else(resource_closed)?;
        let value = (*value).downcast_ref::<T>().ok_or_else(mistyped)?;
        Ok(read(value))
    }

    /// Hold the value for the rest of the call, for a method that awaits: the guard reads as the
    /// value and keeps it alive while held, so a `&T` borrowed from it stays valid across the
    /// method's awaits, as a component's `&self` does while its method is suspended. Held by the
    /// method's future, it lasts until the call ends, also when the issuing object is reloaded or
    /// released meanwhile. A value that has closed is refused as `Gone/resource`.
    pub fn guard(&self) -> Result<Guard<'_, T>> {
        let value = self
            .issuer
            .upgrade()
            .and_then(|issuer| issuer.value(self.key))
            .ok_or_else(resource_closed)?;
        if !(*value).is::<T>() {
            return Err(mistyped());
        }
        Ok(Guard {
            value,
            receiver: PhantomData,
        })
    }
}

/// A method's `self` held for the rest of its call, from [`Receiver::guard`]: it reads as the value,
/// which it keeps alive while held. It borrows the receiver, which the call took from its payload.
pub struct Guard<'a, T> {
    value: Rc<dyn Any>,
    receiver: PhantomData<&'a Receiver<T>>,
}

impl<T: Issuable> std::ops::Deref for Guard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        (*self.value)
            .downcast_ref::<T>()
            .expect("a guard holds a value of its receiver's type")
    }
}

/// The data slot of a capability issued by [`Payload::issue`]. It serializes as the capability's
/// payload-local index, wherever the result's data names the resource: the whole result, a
/// record field, a list element, a tuple position, an option, or a variant or result payload.
#[must_use = "a slot names its capability in the result's data"]
pub struct Slot(u32);

impl Serialize for Slot {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.0)
    }
}

/// The payload-local index, for a result whose data is written without serde.
impl From<Slot> for u32 {
    fn from(slot: Slot) -> Self {
        slot.0
    }
}

/// The implicit `self` of one call. The managed call holds its physical owner separately;
/// retaining this weak loan cannot extend the resource's lifetime.
pub struct Loan(Weak<dyn Capability>);

impl Loan {
    /// Inspect the issuer's backend while it is alive; another issuer or an expired loan
    /// yields `None`. The backend reference cannot escape the operation.
    pub fn with_backend<T: Capability, R>(&self, operation: impl FnOnce(&T) -> R) -> Option<R> {
        let owner = self.0.upgrade()?;
        (&*owner as &dyn Any).downcast_ref::<T>().map(operation)
    }
}

/// The one internal payload of portable calls: data plus capabilities addressed by
/// payload-local indices. A bare number in `value` names nothing outside this payload.
pub struct Payload {
    value: Value,
    owned: Vec<Issued>,
    moved: Vec<Moved>,
    receiver: Option<Loan>,
    streams: Vec<ByteStream>,
    /// The object version serving the call these arguments enter, stamped by the managed call.
    issuer: Option<Weak<Issuer>>,
}

impl Payload {
    /// A payload carrying only data.
    pub fn data(value: Value) -> Self {
        Self {
            value,
            owned: Vec::new(),
            moved: Vec::new(),
            receiver: None,
            streams: Vec::new(),
            issuer: None,
        }
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    /// Replace the data in place, keeping every capability.
    pub fn map_value(mut self, map: impl FnOnce(Value) -> Result<Value>) -> Result<Self> {
        self.value = map(std::mem::take(&mut self.value))?;
        Ok(self)
    }

    /// Attach an owned capability and return its payload-local index for the data slot.
    pub fn attach(&mut self, issued: Issued) -> u32 {
        self.owned.push(issued);
        u32::try_from(self.owned.len() - 1).expect("payload attachment count fits u32")
    }

    /// Move an existing resource into an argument. The caller relinquishes its owner now;
    /// refused admission consumes and closes it rather than recreating a Wasm handle.
    pub fn attach_argument(&mut self, owner: Owned) -> Result<u32> {
        Ok(self.attach_moved(Moved::from_shared(Rc::new(owner))?))
    }

    /// Attach a move already redeemed by a trusted executor, without duplicating its claim.
    pub fn attach_moved(&mut self, moved: Moved) -> u32 {
        self.moved.push(moved);
        u32::try_from(self.moved.len() - 1).expect("payload attachment count fits u32")
    }

    /// Consume the typed owned arguments; their data positions are checked by the executor.
    pub fn take_arguments(&mut self) -> Vec<Moved> {
        std::mem::take(&mut self.moved)
    }

    /// Attach a stream end and return its payload-local index for the data slot.
    pub fn attach_stream(&mut self, stream: ByteStream) -> u32 {
        self.streams.push(stream);
        u32::try_from(self.streams.len() - 1).expect("payload stream count fits u32")
    }

    /// Take the implicit `self` loan of this call, if any.
    pub fn take_receiver(&mut self) -> Option<Loan> {
        self.receiver.take()
    }

    /// Issue a Rust value as a capability of its WIT resource type. Place the returned [`Slot`]
    /// where the result's data names the resource, and serialize that result with
    /// [`Payload::encode`]. The value is this payload's until the call returns it: then it
    /// belongs to the issuing object, and the delivered handle names it.
    pub fn issue<T: Issuable>(&mut self, value: T) -> Slot {
        Slot(self.attach(Issued::new(Backend {
            nominal: T::nominal(),
            place: RefCell::new(Place::Pending(Rc::new(value))),
        })))
    }

    /// Replace the data with a serialized result, whose resource positions hold the slots this
    /// payload issued.
    pub fn encode(self, result: impl Serialize) -> Result<Self> {
        self.map_value(|_| {
            serde_json::to_value(result)
                .map_err(|error| Error::invalid(Reason::Result, error.to_string()))
        })
    }

    /// Take the implicit `self` of this call as the value it was issued from. A call without
    /// `self` is refused, as is a `self` of another resource type (`Forbidden/resource`), of
    /// another object or a retired revision of the exporter, or one that has closed
    /// (`Gone/resource`).
    pub fn receiver<T: Issuable>(&mut self) -> Result<Receiver<T>> {
        let loan = self.receiver.take().ok_or_else(|| {
            Error::invalid(
                Reason::Resource,
                "a resource method requires its owned handle",
            )
        })?;
        let owner = loan.0.upgrade().ok_or_else(resource_closed)?;
        let Some(backend) = (&*owner as &dyn Any).downcast_ref::<Backend>() else {
            return Err(if *owner.nominal() == T::nominal() {
                another_issuer()
            } else {
                mistyped()
            });
        };
        if backend.nominal != T::nominal() {
            return Err(mistyped());
        }
        let (issuer, key) = match &*backend.place.borrow() {
            Place::Issued { issuer, key } => (issuer.clone(), *key),
            Place::Pending(_) => return Err(another_issuer()),
        };
        if !self
            .issuer
            .as_ref()
            .is_some_and(|current| Weak::ptr_eq(current, &issuer))
        {
            return Err(another_issuer());
        }
        let receiver = Receiver {
            issuer,
            key,
            marker: PhantomData,
        };
        receiver.with(|_| ())?;
        Ok(receiver)
    }

    /// Whether the payload carries no capability at all.
    pub fn is_data(&self) -> bool {
        self.receiver.is_none()
            && self.owned.is_empty()
            && self.moved.is_empty()
            && self.streams.is_empty()
    }

    /// Return the data of a call to an exporter's function without `self`: a `self` lent to it
    /// is refused as a compiled exporter's function refuses it, `Invalid/resource`, and any other
    /// capability as a data-only boundary refuses it, `Invalid/local_value`. Refused, they drop
    /// and so close.
    pub fn into_data(self) -> Result<Value> {
        if self.receiver.is_some() {
            return Err(Error::invalid(
                Reason::Resource,
                "this function takes no resource receiver",
            ));
        }
        self.data_only()
    }

    /// Return the data of a data-only payload, as a data-only boundary or a method taking data
    /// only reads it: every capability, a lent `self` too, is refused as `Invalid/local_value`,
    /// which drops and so closes it.
    pub(crate) fn data_only(self) -> Result<Value> {
        if self.is_data() {
            Ok(self.value)
        } else {
            Err(capability_refused())
        }
    }

    /// Bind every stream end of a result to the calling root, once, before any consumer
    /// decoder sees the payload: whatever the decoder keeps, the root's end closes it and its
    /// cancellation cancels it. A data result does nothing here.
    pub(crate) fn bind_streams(&self, origin: &Origin) -> Result<()> {
        for stream in &self.streams {
            origin.bind(stream)?;
        }
        Ok(())
    }

    pub(crate) fn with_receiver(mut self, loan: Loan) -> Result<Self> {
        if self.receiver.is_some() {
            return Err(Error::invalid(
                Reason::Arguments,
                "a call has at most one implicit self",
            ));
        }
        self.receiver = Some(loan);
        Ok(self)
    }

    /// Enter the call that `issuer`'s object version serves. Newly issued capabilities cannot
    /// become arguments without a calling claim. Existing moved owners keep their exact claim.
    /// Pin implicit self outside the provider's payload so retaining that payload cannot keep a
    /// resource alive after the physical call settles.
    pub(crate) fn for_call(
        mut self,
        issuer: &Rc<Issuer>,
    ) -> Result<(Self, Option<Rc<dyn Capability>>)> {
        self.issuer = Some(Rc::downgrade(issuer));
        if !self.owned.is_empty() {
            return Err(Error::invalid(
                Reason::Arguments,
                "an issued result cannot become an argument without an ownership claim",
            ));
        }
        if !self.streams.is_empty() {
            return Err(Error::invalid(
                Reason::Arguments,
                "stream arguments are not supported",
            ));
        }
        let owner = self
            .receiver
            .as_ref()
            .map(|loan| loan.0.upgrade().ok_or_else(resource_closed))
            .transpose()?;
        Ok((self, owner))
    }

    /// Give every value this result issued to the object version returning it: from here on
    /// its last owner removes it from that object, and the object's end closes it.
    pub(crate) fn issued_by(self, issuer: &Rc<Issuer>) -> Self {
        for issued in &self.owned {
            if let Some(backend) = (&*issued.0 as &dyn Any).downcast_ref::<Backend>() {
                backend.adopt(issuer);
            }
        }
        self
    }
}

fn resource_closed() -> Error {
    Error::new(core::Code::Gone, Reason::Resource, "resource has closed")
}

fn another_issuer() -> Error {
    Error::new(
        core::Code::Gone,
        Reason::Resource,
        "the resource belongs to another object or a retired revision",
    )
}

fn mistyped() -> Error {
    Error::new(
        core::Code::Forbidden,
        Reason::Resource,
        "the resource has another type",
    )
}

/// Refusal of capabilities by a data-only boundary (JSON, RPC or a foreign host).
pub(crate) fn capability_refused() -> Error {
    Error::invalid(
        Reason::LocalValue,
        "a data-only boundary cannot carry host capabilities",
    )
}

/// Arguments of a portable call: ordinary Serde values, or a payload moved as is.
pub trait IntoPayload {
    fn into_payload(self) -> Result<Payload>;
}

impl<T: Serialize> IntoPayload for T {
    fn into_payload(self) -> Result<Payload> {
        serde_json::to_value(&self)
            .map(Payload::data)
            .map_err(|error| Error::invalid(Reason::Arguments, error.to_string()))
    }
}

impl IntoPayload for Payload {
    fn into_payload(self) -> Result<Payload> {
        if self.receiver.is_some() {
            return Err(Error::invalid(
                Reason::Arguments,
                "a borrowed receiver cannot be forwarded",
            ));
        }
        Ok(self)
    }
}

/// Results of a portable call. Decoding runs inside the provider's managed call, before the
/// kernel invocation finishes, so ownership exists before delivery can fail.
pub trait FromPayload: Sized {
    fn from_payload(payload: Payload, origin: &Origin) -> Result<Self>;
}

impl<T: DeserializeOwned> FromPayload for T {
    fn from_payload(payload: Payload, _: &Origin) -> Result<Self> {
        serde_json::from_value(payload.data_only()?)
            .map_err(|error| Error::invalid(Reason::Result, error.to_string()))
    }
}

impl FromPayload for Owned {
    /// A bare `own` result: the data names the only attachment.
    fn from_payload(mut payload: Payload, origin: &Origin) -> Result<Self> {
        let index = payload
            .value
            .as_u64()
            .and_then(|index| usize::try_from(index).ok());
        let single = payload.receiver.is_none()
            && payload.moved.is_empty()
            && payload.streams.is_empty()
            && payload.owned.len() == 1;
        let issued = index
            .filter(|index| single && *index == 0)
            .and_then(|_| payload.owned.pop())
            .ok_or_else(|| {
                Error::invalid(
                    Reason::Result,
                    "an owned result names exactly one capability",
                )
            })?;
        Owned::anchor(issued, origin)
    }
}

/// A payload result for executors: data, every owned capability, each anchored here, and every
/// stream end, already bound to the calling root by the call. A data-only result allocates no
/// claim and moves its value without conversion; a stream end takes no claim either.
pub struct Delivered {
    pub value: Value,
    pub owned: Vec<Owned>,
    pub streams: Vec<ByteStream>,
}

impl FromPayload for Delivered {
    fn from_payload(payload: Payload, origin: &Origin) -> Result<Self> {
        let Payload {
            value,
            owned,
            moved,
            receiver,
            streams,
            issuer: _,
        } = payload;
        if receiver.is_some() || !moved.is_empty() {
            return Err(Error::invalid(
                Reason::Result,
                "a result cannot carry argument moves or lend self",
            ));
        }
        let owned = owned
            .into_iter()
            .map(|issued| Owned::anchor(issued, origin))
            .collect::<Result<_>>()?;
        Ok(Self {
            value,
            owned,
            streams,
        })
    }
}

impl FromPayload for ByteStream {
    /// A whole `stream<u8>` result: the data names the only attachment, a stream end. The call
    /// already bound it to the calling root before decoding.
    fn from_payload(mut payload: Payload, _: &Origin) -> Result<Self> {
        let whole = payload.receiver.is_none()
            && payload.owned.is_empty()
            && payload.moved.is_empty()
            && payload.streams.len() == 1
            && payload.value.as_u64() == Some(0);
        let stream = payload.streams.pop().filter(|_| whole).ok_or_else(|| {
            Error::invalid(Reason::Result, "a stream result names exactly one stream")
        })?;
        Ok(stream)
    }
}

/// The calling claim of one portable call, as seen by its result decoding.
pub struct Origin {
    host: Host,
    reference: core::Id,
    object: core::Id,
    foreign: bool,
    root: core::Id,
}

impl Origin {
    pub(crate) fn new(
        host: Host,
        reference: core::Id,
        object: core::Id,
        foreign: bool,
        root: core::Id,
    ) -> Self {
        Self {
            host,
            reference,
            object,
            foreign,
            root,
        }
    }

    /// Bind a decoded stream end to the calling root; refused once that root has ended or was
    /// cancelled, which drops and so closes the end.
    fn bind(&self, stream: &ByteStream) -> Result<()> {
        if self.foreign {
            return Err(capability_refused());
        }
        self.host.bind_root_stream(self.root, stream.root_stream())
    }

    /// An exact duplicate of the calling claim; refused once that claim was released.
    fn anchor(&self) -> Result<Reference> {
        if self.foreign {
            return Err(capability_refused());
        }
        self.host.duplicate_reference(self.reference, self.object)
    }
}

/// An owned WIT resource handle held by native code. Dropping it is the WIT drop. It is not
/// `Clone`. It closes its capability first and then releases its own admission claim.
pub struct Owned {
    capability: RefCell<Option<Rc<dyn Capability>>>,
    anchor: RefCell<Option<Rc<Reference>>>,
    nominal: Nominal,
}

/// One linear move of an existing owner. It retains the exact original admission claim;
/// receiving it creates a new live wrapper, while all aliases of the sender stay revoked.
/// The capability is closed only after this move and any physical borrows have ended.
pub struct Moved {
    capability: Rc<dyn Capability>,
    anchor: Rc<Reference>,
}

impl Moved {
    /// Revoke a table-owned sender before its admission can suspend. Existing physical calls
    /// keep their own pins, but no alias can issue another method after the move.
    pub fn from_shared(owner: Rc<Owned>) -> Result<Self> {
        let capability = owner
            .capability
            .borrow_mut()
            .take()
            .ok_or_else(moved_owner)?;
        let anchor = owner
            .anchor
            .borrow_mut()
            .take()
            .expect("a live owner has its claim");
        Ok(Self { capability, anchor })
    }

    pub fn nominal(&self) -> &Nominal {
        self.capability.nominal()
    }

    /// Inspect the original issuer backend without exposing or duplicating its authority.
    pub fn with_backend<T: Capability, R>(&self, operation: impl FnOnce(&T) -> R) -> Option<R> {
        (&*self.capability as &dyn Any)
            .downcast_ref::<T>()
            .map(operation)
    }

    /// Receive the moved owner under the same claim, never under the receiving Orb's claim.
    pub fn receive(self) -> Owned {
        let nominal = self.capability.nominal().clone();
        Owned {
            capability: RefCell::new(Some(self.capability)),
            anchor: RefCell::new(Some(self.anchor)),
            nominal,
        }
    }
}

impl Owned {
    /// Bind an issued capability to its own duplicate of the calling claim. If the claim is
    /// already gone, the capability is dropped, and so closed, with the error.
    fn anchor(issued: Issued, origin: &Origin) -> Result<Self> {
        let anchor = origin.anchor()?;
        let nominal = issued.0.nominal().clone();
        Ok(Self {
            capability: RefCell::new(Some(issued.0)),
            anchor: RefCell::new(Some(Rc::new(anchor))),
            nominal,
        })
    }

    fn claim(&self) -> Result<(Rc<dyn Capability>, Rc<Reference>)> {
        let capability = self.capability.borrow().clone().ok_or_else(moved_owner)?;
        let anchor = self.anchor.borrow().clone().ok_or_else(moved_owner)?;
        Ok((capability, anchor))
    }

    pub fn nominal(&self) -> &Nominal {
        &self.nominal
    }

    /// Call a method of this resource. The call is admitted on this handle's own claim with
    /// every ordinary kernel check. The handle travels only as the call's implicit `self` loan.
    pub async fn call_portable<A: IntoPayload + 'static, R: FromPayload + 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
    ) -> Result<R> {
        let (capability, anchor) = self.claim()?;
        let payload = arguments
            .into_payload()?
            .with_receiver(Loan(Rc::downgrade(&capability)))?;
        anchor.call_borrowed(context, method, payload).await
    }

    /// Like [`Owned::call_portable`], with control of the admitted invocation: see
    /// [`crate::Reference::issue_portable`].
    pub async fn issue_portable<A: IntoPayload + 'static, R: FromPayload + 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
    ) -> Result<(crate::IssuedCall, LocalBoxFuture<'static, Result<R>>)> {
        let (capability, anchor) = self.claim()?;
        let payload = arguments
            .into_payload()?
            .with_receiver(Loan(Rc::downgrade(&capability)))?;
        anchor.issue_borrowed(context, method, payload).await
    }
}

fn moved_owner() -> Error {
    Error::new(
        core::Code::Gone,
        Reason::Resource,
        "resource ownership has moved",
    )
}

/// What a portable provider returns: its logical result and, when the call keeps working after
/// publishing it, that physical work.
///
/// The caller receives only the result. The physical work stays with the call's managed task,
/// which holds the kernel invocation until the work ends; it is never part of a [`Payload`].
pub struct Outcome {
    head: Result<Payload>,
    physical: Option<LocalBoxFuture<'static, Result<()>>>,
}

impl Outcome {
    /// A result published before the call's physical work has ended. The work is driven to
    /// its end even if the result is refused or nobody receives it.
    pub fn continuing(
        head: Result<Payload>,
        physical: impl Future<Output = Result<()>> + 'static,
    ) -> Self {
        Self {
            head,
            physical: Some(physical.boxed_local()),
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (Result<Payload>, Option<LocalBoxFuture<'static, Result<()>>>) {
        (self.head, self.physical)
    }
}

/// An ordinary result: the call has physically ended when it returns.
impl From<Payload> for Outcome {
    fn from(payload: Payload) -> Self {
        Self {
            head: Ok(payload),
            physical: None,
        }
    }
}
