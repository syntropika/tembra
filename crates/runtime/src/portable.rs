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
//! A payload may also carry the end of a byte stream ([`ByteStream`]). The physical work that
//! produces it never travels in a payload: a provider returns it beside its result in an
//! [`Outcome`], and the call's own managed task keeps driving it under the call's invocation.
use crate::{Context, Error, Host, Reference, Result, core, core::Reason};
use futures::{FutureExt, future::LocalBoxFuture};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    any::Any,
    cell::RefCell,
    future::Future,
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

    /// Whether the payload carries no capability at all.
    pub fn is_data(&self) -> bool {
        self.receiver.is_none()
            && self.owned.is_empty()
            && self.moved.is_empty()
            && self.streams.is_empty()
    }

    /// Return the data of a data-only payload. Capabilities are refused, which drops and so
    /// closes them.
    pub fn into_data(self) -> Result<Value> {
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

    /// Newly issued capabilities cannot become arguments without a calling claim. Existing
    /// moved owners keep their exact claim. Pin implicit self outside the provider's payload so
    /// retaining that payload cannot keep a resource alive after the physical call settles.
    pub(crate) fn for_call(self) -> Result<(Self, Option<Rc<dyn Capability>>)> {
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
            .map(|loan| {
                loan.0.upgrade().ok_or_else(|| {
                    Error::new(core::Code::Gone, Reason::Resource, "resource has closed")
                })
            })
            .transpose()?;
        Ok((self, owner))
    }
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
        serde_json::from_value(payload.into_data()?)
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
