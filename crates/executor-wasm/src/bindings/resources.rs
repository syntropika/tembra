//! WIT resources of compiled revisions at the native capability boundary.
//!
//! **Exporter.** A function whose result is a whole `own<R>` returns a host-issued instance id
//! from its Store. It immediately becomes an [`Issued`] capability backed by a
//! [`GuestInstance`]. A method receives its implicit `self` only as the call's [`Loan`], which is
//! redeemed here: the loan must come from this very environment and have the method's
//! resource type. Its instance id never comes from the guest or a JSON value.
//!
//! **Importer.** An owned result is installed in the issuing table as [`Claim::Owned`], and its
//! slot is the guest's dynamic handle representation. Installation is committed only after the
//! worker has received the reply; otherwise it is rolled back, which closes the handle.
//! A reference view instead names a root-owned kernel claim in that same table. Its data-only
//! methods select that claim; a drop or abandoned delivery releases it without transferring any
//! provider resource between Orbs.
//!
//! Every call still goes through the kernel: constructors and factories through the import's
//! reference, and methods through the owned handle's own admission claim.
use super::*;
use crate::worker::Reply;
use serde_json::json;
use tembra::portable::{Capability, Delivered, Issued, Loan, Nominal, Owned, Payload};

/// Resource positions of one bound function: the implicit `self` and a whole owned result.
#[derive(Clone, Default)]
pub(crate) struct Roles {
    pub receiver: Option<Nominal>,
    pub own_result: Option<Nominal>,
    /// The whole result is a `stream<u8>`, served by [`Environment::invoke_stream`].
    pub stream: bool,
}

impl Roles {
    /// Read the checked positions of a function of a revision admitted to the capability
    /// transport.
    pub(crate) fn of(contract: &tembra_contracts::PreparedContract, slot: u32) -> Result<Self> {
        let signature = contract.resource_signature(slot).map_err(failure)?;
        Ok(Self {
            receiver: signature.receiver.map(nominal).transpose()?,
            own_result: signature.own_result.map(nominal).transpose()?,
            stream: signature.stream_result,
        })
    }

    pub(crate) fn is_data(&self) -> bool {
        self.receiver.is_none() && self.own_result.is_none() && !self.stream
    }
}

/// Only a resource defined by a named interface has a portable identity.
pub(crate) fn nominal(identity: &tembra_contracts::ResourceIdentity) -> Result<Nominal> {
    match (&identity.interface, &identity.name) {
        (Some(interface), Some(name)) => Ok(Nominal {
            interface: interface.clone(),
            name: name.clone(),
        }),
        _ => Err(failure("an anonymous resource has no portable identity")),
    }
}

/// One exported guest resource, held by its native owner.
///
/// The issuer edge is weak, so an owner never keeps a released environment or a retired
/// revision alive. Dropping the last owner asks the issuing Store to destroy the instance. If
/// that environment was already released, its drain has destroyed the instance already.
struct GuestInstance {
    nominal: Nominal,
    issuer: Weak<Handles>,
    environment: u64,
    instance: u32,
}

impl Capability for GuestInstance {
    fn nominal(&self) -> &Nominal {
        &self.nominal
    }
}

impl Drop for GuestInstance {
    fn drop(&mut self) {
        let Some(issuer) = self.issuer.upgrade() else {
            return;
        };
        match holding(issuer.root.as_ref()) {
            // The destructor may drop imported resources the issuing revision holds.
            Some(root) => issuer.session.destroy(
                self.environment,
                self.instance,
                Some(&mut |slot| {
                    release_slot(&root, slot);
                }),
            ),
            None => issuer
                .session
                .destroy(self.environment, self.instance, None),
        }
    }
}

/// The Orb table, if it holds imported resources or views a destructor could drop.
pub(super) fn holding(root: Option<&Weak<Handles>>) -> Option<Rc<Handles>> {
    root.and_then(Weak::upgrade).filter(|root| {
        root.references
            .borrow()
            .values()
            .any(|handle| matches!(handle.claim, Claim::Owned(_) | Claim::View { .. }))
    })
}

/// Remove a local owned slot or reference view, leaving factory declarations alone. Release
/// outside the table borrow, because closing a handle reaches the kernel and its issuer.
pub(super) fn release_slot(table: &Handles, slot: u32) -> bool {
    let removed = {
        let mut references = table.references.borrow_mut();
        match references.get(&slot) {
            Some(ReferenceHandle {
                claim: Claim::Owned(_),
                ..
            }) => references.remove(&slot),
            Some(ReferenceHandle {
                claim: Claim::View { .. },
                ..
            }) => references.remove(&slot),
            _ => None,
        }
    };
    let released = removed.is_some();
    if let Some(ReferenceHandle {
        claim: Claim::View { reference, .. },
        ..
    }) = removed
        && let Some(reference) = reference.upgrade()
    {
        let _ = reference.release();
    }
    released
}

/// Refusal of a call whose resource positions do not match its function.
fn misuse(message: &str) -> tembra::Error {
    tembra::Error::new(tembra::core::Code::Invalid, "resource", message)
}

/// Refusal of a resource of another type than the function's.
fn mistyped() -> tembra::Error {
    tembra::Error::new(
        tembra::core::Code::Forbidden,
        "resource",
        "the resource has another type",
    )
}

/// Refusal of a receiver that this environment did not issue.
fn foreign_receiver() -> tembra::Error {
    tembra::Error::new(
        tembra::core::Code::Gone,
        "resource",
        "the resource belongs to another instance or a retired revision",
    )
}

impl Environment {
    /// Serve one export function with resource positions.
    pub(crate) async fn invoke_resource(
        &self,
        context: Context,
        slot: u32,
        roles: &Roles,
        mut payload: Payload,
    ) -> Result<Payload> {
        // The managed call pins the receiver's owner until it has physically finished.
        let loan = payload.take_receiver();
        let receiver = match (&roles.receiver, &loan) {
            (Some(expected), Some(loan)) => Some(self.redeem(loan, expected)?),
            (None, None) => None,
            (Some(_), None) => return Err(misuse("a resource method requires its owned handle")),
            (None, Some(_)) => return Err(misuse("this function takes no resource receiver")),
        };
        // Any other attachment is refused, which closes it.
        let args = self
            .contract
            .to_resource_wire(slot, true, payload.into_data()?)
            .map_err(failure)?;
        let context = context.enter_execution()?;
        let cancellation = context.signal();
        let packet = GuestCommand::Invoke {
            environment: self.id,
            function: slot,
            args: &args,
            receiver,
        };
        let value = self
            .execute(
                Scope::Call(context),
                Some(cancellation),
                packet,
                Carrier::None,
            )
            .await?;
        drop(loan);
        let Some(nominal) = &roles.own_result else {
            let value = self
                .contract
                .from_resource_wire(slot, false, value)
                .map_err(failure)?;
            return Ok(Payload::data(value));
        };
        let instance = value
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| failure("compiled resource result is not an instance id"))?;
        // Owned before anything else can fail, so a refused result is destroyed, not leaked.
        let issued = Issued::new(GuestInstance {
            nominal: nominal.clone(),
            issuer: Rc::downgrade(&self.handles),
            environment: self.id,
            instance,
        });
        let mut result = Payload::data(Value::Null);
        let index = result.attach(issued);
        result.map_value(|_| {
            self.contract
                .from_resource_wire(slot, false, json!(index))
                .map_err(failure)
        })
    }

    /// Redeem a `self` loan: a live compiled instance issued by this very environment, of the
    /// method's resource type. Only the copied instance id leaves the inspection.
    fn redeem(&self, loan: &Loan, expected: &Nominal) -> Result<u32> {
        let issuer = Rc::downgrade(&self.handles);
        let (nominal, issued, instance) = loan
            .with_backend::<GuestInstance, _>(|backend| {
                (
                    backend.nominal == *expected,
                    backend.environment == self.id && Weak::ptr_eq(&backend.issuer, &issuer),
                    backend.instance,
                )
            })
            .ok_or_else(foreign_receiver)?;
        if !nominal {
            return Err(mistyped());
        }
        if !issued {
            return Err(foreign_receiver());
        }
        Ok(instance)
    }

    /// Forward one guest import call. The handle the guest named selects the callee: a service
    /// reference for freestanding functions, constructors and statics, or an owned resource for
    /// its methods.
    ///
    /// `stream` is set when the request came on the stream carrier, whose pipe receives the
    /// result. It must match the import's checked signature before anything is issued: a data
    /// import is never called as a stream, and a stream import never without its pipe.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn call_import(
        &self,
        scope: &Scope,
        owner: Option<ScopeId>,
        id: u32,
        method: &str,
        args: Value,
        stream: bool,
        issued: &crate::worker::Issued,
    ) -> Result<Reply> {
        let context = Self::context(scope)?;
        let (callee, binding) = self.callee(scope, owner, id)?;
        let method = binding
            .methods
            .get(method)
            .ok_or_else(|| failure("unknown prepared import method"))?;
        let roles = &method.roles;
        match (roles.stream, stream) {
            (true, false) => {
                return Err(misuse(
                    "a byte-stream import is called only on its stream carrier",
                ));
            }
            (false, true) => return Err(misuse("the import does not return a byte stream")),
            _ => {}
        }
        let args = if roles.is_data() {
            self.contract.from_wire(method.slot, true, args)
        } else {
            self.contract.from_resource_wire(method.slot, true, args)
        }
        .map_err(failure)?;
        // The payload moves without re-encoding; a data result allocates no claim.
        let payload = Payload::data(args);
        let Delivered {
            value,
            owned,
            mut streams,
        } = match (callee, &roles.receiver) {
            // The admitted call is handed to the request first, then awaited.
            (Callee::Reference(reference), receiver)
                if receiver.is_none() || receiver.as_ref() == binding.reference.as_ref() =>
            {
                let (call, result) = reference
                    .issue_portable::<_, Delivered>(context, &method.name, payload)
                    .await?;
                issued.set(Some(call));
                result.await?
            }
            (Callee::Owned(handle), Some(expected))
                if binding.reference.is_none() && handle.nominal() == expected =>
            {
                let (call, result) = handle
                    .issue_portable::<_, Delivered>(context, &method.name, payload)
                    .await?;
                issued.set(Some(call));
                result.await?
            }
            (Callee::Owned(_), Some(_)) => return Err(mistyped()),
            (Callee::Owned(_), None) => {
                return Err(misuse("a resource handle calls only its own methods"));
            }
            (Callee::Reference(_), _) => {
                return Err(misuse("a resource method requires its owned handle"));
            }
        };
        if roles.stream {
            // The whole result is one stream end, filled into the importer's pipe after the
            // worker has received this reply.
            let end = streams.pop();
            return match (
                end,
                owned.is_empty() && streams.is_empty() && value == json!(0),
            ) {
                (Some(end), true) => Ok(Reply {
                    value: json!(0),
                    delivery: None,
                    stream: Some(end),
                }),
                _ => Err(failure("a stream result names exactly one stream")),
            };
        }
        if !streams.is_empty() {
            return Err(failure("the result carries an unexpected stream"));
        }
        let Some(expected) = &roles.own_result else {
            if !owned.is_empty() {
                return Err(failure("the result carries an unexpected resource"));
            }
            let value = if roles.is_data() {
                self.contract.to_wire(method.slot, false, value)
            } else {
                self.contract.to_resource_wire(method.slot, false, value)
            }
            .map_err(failure)?;
            return Ok(value.into());
        };
        let mut owned = owned.into_iter();
        let (Some(handle), None) = (owned.next(), owned.next()) else {
            return Err(failure("an owned result names exactly one resource"));
        };
        if value != json!(0) || handle.nominal() != expected {
            return Err(failure("the owned result has another type"));
        }
        let (slot, delivery) = self.install(handle, binding.clone())?;
        let value = self
            .contract
            .to_resource_wire(method.slot, false, json!(slot))
            .map_err(failure)?;
        Ok(Reply {
            value,
            delivery: Some(delivery),
            stream: None,
        })
    }

    /// Resolve the handle a guest call named. Owned slots are not tied to a root; every other
    /// handle keeps the existing reference rules.
    fn callee(
        &self,
        scope: &Scope,
        owner: Option<ScopeId>,
        id: u32,
    ) -> Result<(Callee, Rc<Import>)> {
        let owned = self
            .selected_handles(owner)?
            .references
            .borrow()
            .get(&id)
            .and_then(|handle| match &handle.claim {
                Claim::Owned(owned) => Some((owned.clone(), handle.import.clone())),
                _ => None,
            });
        if let Some((owned, import)) = owned {
            return Ok((Callee::Owned(owned), import));
        }
        let (reference, import) = self.reference(scope, owner, id)?;
        Ok((Callee::Reference(reference), import))
    }

    /// Install an owned result in the Orb root table of this revision.
    ///
    /// Ownership rule (G1): a compiled guest's state is shared by every object of its Store, so
    /// it may keep a handle in a global and use or drop it during another object's call. The
    /// handle therefore belongs to the Store's revision, not to the object whose call received
    /// it. It lives until the guest drops it or the revision's root environment is released.
    /// Owned slots and declared references share the root table's never-reused id sequence, so
    /// a slot number names exactly one entry. The returned delivery removes the slot again
    /// unless it is committed.
    fn install(&self, handle: Owned, import: Rc<Import>) -> Result<(u32, Delivery)> {
        let table = self.parent.as_ref().unwrap_or(&self.handles);
        let slot = table.allocate()?;
        table.references.borrow_mut().insert(
            slot,
            ReferenceHandle {
                claim: Claim::Owned(Rc::new(handle)),
                import,
            },
        );
        Ok((slot, Delivery::new(table, slot)))
    }

    /// Remove an imported handle or view dropped by the guest, leaving declarations alone.
    ///
    /// A dropped imported handle's representation is always an owned slot of the Orb root
    /// table, and that table's ids are never reused, so a release never reaches a declared
    /// reference by accident.
    pub(super) fn release_import(&self, owner: Option<ScopeId>, id: u32) -> Result<bool> {
        Ok(release_slot(self.selected_handles(owner)?, id))
    }
}

/// The callee a guest import call selected.
enum Callee {
    Reference(Rc<Reference>),
    Owned(Rc<Owned>),
}

/// Owned slots installed for one reply, removed again unless the worker received it.
pub(crate) struct Delivery {
    table: Weak<Handles>,
    slots: Vec<u32>,
}

impl Delivery {
    /// Roll back an unpublished local capability, including its kernel claim.
    pub(super) fn new(table: &Rc<Handles>, slot: u32) -> Self {
        Self {
            table: Rc::downgrade(table),
            slots: vec![slot],
        }
    }

    /// The guest received the reply and now owns the installed handles.
    pub(crate) fn commit(mut self) {
        self.slots.clear();
    }
}

impl Drop for Delivery {
    /// Remove undelivered slots, which closes their handles exactly once.
    fn drop(&mut self) {
        if self.slots.is_empty() {
            return;
        }
        if let Some(table) = self.table.upgrade() {
            for slot in self.slots.drain(..) {
                release_slot(&table, slot);
            }
        }
    }
}
