//! Bind guest operations to issued declarations and contexts on the native host thread.
use crate::{
    failure,
    protocol::{GuestCommand, ScopeId},
    worker::{Carried, Carrier, Session},
};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
};
use tembra::{
    AcquisitionOptions, Cancellation, Context, Declaration, Reference, Resource, ResourceSignal,
    Result, State,
};

mod operations;
mod resources;
mod streams;
pub(crate) use resources::{Delivery, Roles};
pub(crate) use streams::pump;

#[derive(Clone)]
pub(crate) enum Scope {
    Declaration(Declaration),
    Call(Context),
    Open(ResourceSignal),
    Validation,
}

pub(crate) struct Import {
    pub service: String,
    pub methods: BTreeMap<String, BoundMethod>,
    /// The installed requirement's API version and methods, applied to runtime acquisition.
    pub requirement: Option<AcquisitionOptions>,
}
pub(crate) struct BoundMethod {
    pub name: String,
    pub slot: u32,
    /// Resource positions of this function; empty unless the capability transport is selected.
    pub roles: Roles,
}

#[derive(Clone)]
struct ReferenceHandle {
    claim: Claim,
    import: Rc<Import>,
}

/// Who owns the kernel claim behind a guest reference handle.
#[derive(Clone)]
enum Claim {
    /// Declared by a factory: the environment owns the claim for its whole lifetime.
    Declared(Rc<Reference>),
    /// Acquired by `Context.get`: the root's native claim list owns it and releases it when the
    /// root ends. The table only names it, so a long-lived environment never retains claims of
    /// ended roots; only callbacks of the same root may use the handle.
    Root {
        reference: Weak<Reference>,
        root: tembra::core::Id,
    },
    /// An imported WIT resource returned to this table: the slot owns the handle until the guest
    /// drops it or the environment is released. Calls clone the `Rc` before awaiting, so a drop
    /// during a call takes effect when that call has physically finished.
    Owned(Rc<tembra::portable::Owned>),
}

impl Claim {
    /// A root claim whose root has ended no longer names a reference.
    fn expired(&self) -> bool {
        matches!(self, Self::Root { reference, .. } if reference.strong_count() == 0)
    }
}

struct Handles {
    session: Session,
    environment: u64,
    /// The Orb root table of a service environment: it holds this revision's owned imports.
    root: Option<Weak<Handles>>,
    next: Cell<u32>,
    states: RefCell<BTreeMap<u32, State<Value>>>,
    references: RefCell<BTreeMap<u32, ReferenceHandle>>,
    resources: RefCell<BTreeMap<u32, Resource<u32>>>,
    hooks: RefCell<BTreeMap<u32, BoundHook>>,
}

impl Drop for Handles {
    /// Release guest closures only after the last physical native owner has settled.
    fn drop(&mut self) {
        match resources::holding(self.root.as_ref()) {
            // Destructors of resources this environment issued may drop imported ones.
            Some(root) => self
                .session
                .release_environment_with(self.environment, &mut |slot| {
                    resources::release_slot(&root, slot);
                }),
            None => self.session.release_environment(self.environment),
        }
    }
}

/// A scope owns its native handles; numeric guest values cannot select another scope.
#[derive(Clone)]
pub(crate) struct Environment {
    pub session: Session,
    pub id: u64,
    handles: Rc<Handles>,
    service: Option<String>,
    imports: Rc<BTreeMap<String, Rc<Import>>>,
    parent: Option<Rc<Handles>>,
    contract: tembra_contracts::PreparedContract,
    hook_bindings: Rc<BTreeMap<String, tembra_contracts::HookBinding>>,
}

impl Environment {
    /// Build one isolated environment using a host-issued declaration capability.
    pub(crate) fn create(
        session: Session,
        id: u64,
        declaration: Declaration,
        service: Option<String>,
        imports: Rc<BTreeMap<String, Rc<Import>>>,
        contract: tembra_contracts::PreparedContract,
        hook_bindings: Rc<BTreeMap<String, tembra_contracts::HookBinding>>,
    ) -> Result<Self> {
        let parent = if service.is_some() {
            Some(declaration.orb::<Environment>()?.handles.clone())
        } else {
            None
        };
        let handles = Rc::new(Handles {
            session: session.clone(),
            environment: id,
            root: parent.as_ref().map(Rc::downgrade),
            next: Cell::new(0),
            states: RefCell::default(),
            references: RefCell::default(),
            resources: RefCell::default(),
            hooks: RefCell::default(),
        });
        let environment = Self {
            session,
            id,
            handles,
            service,
            imports,
            parent,
            contract,
            hook_bindings,
        };
        let scope = Scope::Declaration(declaration);
        environment.session.blocking(
            crate::protocol::Operation::Declare,
            serde_json::to_string(&GuestCommand::Create {
                environment: id,
                service: environment.service.as_deref(),
            })
            .map_err(failure)?,
            |packet, _| environment.sync(&scope, packet),
        )?;
        Ok(environment)
    }

    /// Invoke a guest callback under an actual admitted native context.
    pub(crate) async fn invoke(
        &self,
        context: Context,
        method: &str,
        args: Value,
    ) -> Result<Value> {
        let context = context.enter_execution()?;
        let cancellation = context.signal();
        let packet = GuestCommand::Invoke {
            environment: self.id,
            service: self.service.as_deref(),
            method,
            args: &args,
            receiver: None,
        };
        self.execute(
            Scope::Call(context),
            Some(cancellation),
            packet,
            Carrier::None,
        )
        .await
    }

    /// Serve one incoming HTTP request under an admitted native context.
    ///
    /// The call is an ordinary callback of this revision: it waits for admission, holds the
    /// Store's one callback slot and the kernel pin until it has physically settled, and its
    /// caller's cancellation ends it. Its response head leaves early on the call's own channel.
    pub(crate) async fn ingress(
        &self,
        context: Context,
        method: &str,
        ingress: crate::ingress::Ingress,
    ) -> Result<()> {
        let context = context.enter_execution()?;
        let cancellation = context.signal();
        let packet = GuestCommand::Ingress {
            environment: self.id,
            service: self.service.as_deref(),
            method,
        };
        self.execute(
            Scope::Call(context),
            Some(cancellation),
            packet,
            Carrier::Ingress(Box::new(ingress.0)),
        )
        .await
        .map(|_| ())
    }

    /// Contributions retain the same Orb environment and execution ancestry as service calls.
    pub(crate) async fn contribute(
        &self,
        context: Context,
        binding: &tembra_contracts::ContributionBinding,
        value: Value,
    ) -> Result<Option<Value>> {
        let context = context.enter_execution()?;
        let args = self
            .contract
            .to_wire(binding.slot, true, Value::Array(vec![value]))
            .map_err(failure)?;
        let packet = GuestCommand::Contribute {
            environment: self.id,
            binding: &binding.binding,
            service: &binding.alias,
            method: &binding.method,
            args: &args,
        };
        let value = self
            .execute(
                Scope::Call(context.clone()),
                Some(context.signal()),
                packet,
                Carrier::None,
            )
            .await?;
        let value = self
            .contract
            .from_wire(binding.slot, false, value)
            .map_err(failure)?;
        Ok(if binding.optional && value.is_null() {
            None
        } else {
            Some(value)
        })
    }

    /// Run a callback while routing operations through this environment's owned handle table.
    /// A cancellable callback may leave the admission queue; cleanup passes no cancellation.
    async fn execute(
        &self,
        scope: Scope,
        cancellation: Option<Cancellation>,
        packet: GuestCommand<'_>,
        carrier: Carrier,
    ) -> Result<Value> {
        let admission = self.session.admit(cancellation.as_ref()).await?;
        if cancellation.is_some() {
            // Authority can expire while the callback waits behind another one.
            Self::authorize(&scope)?;
        }
        self.session
            .dispatch(
                admission,
                cancellation.as_ref(),
                packet,
                carrier,
                async |packet, carried, issued| match carried {
                    Carried::Sync => self.sync(&scope, packet).map(Into::into),
                    Carried::Async => self.asynchronous(&scope, packet, false, issued).await,
                    Carried::Stream => self.asynchronous(&scope, packet, true, issued).await,
                },
            )
            .await
    }

    /// Reject a scope whose root, invocation or opener was cancelled or has ended.
    fn authorize(scope: &Scope) -> Result<()> {
        match scope {
            Scope::Call(context) => context.check(),
            Scope::Open(signal) => signal.check(),
            Scope::Declaration(_) | Scope::Validation => Ok(()),
        }
    }

    /// Allocate a handle that is meaningful only inside this environment's host table.
    fn allocate(&self) -> Result<u32> {
        let id = self
            .handles
            .next
            .get()
            .checked_add(1)
            .ok_or_else(|| failure("scope handle space exhausted"))?;
        self.handles.next.set(id);
        Ok(id)
    }

    /// Require the host's live invocation rather than a guest-provided root identifier.
    fn context(scope: &Scope) -> Result<&Context> {
        let Scope::Call(context) = scope else {
            return Err(failure("operation requires an admitted invocation"));
        };
        context.check()?;
        Ok(context)
    }

    /// Refuse declaration operations once the synchronous factory has returned.
    fn declaration(scope: &Scope) -> Result<&Declaration> {
        let Scope::Declaration(declaration) = scope else {
            return Err(failure("operation requires an active declaration"));
        };
        Ok(declaration)
    }

    /// Permit only this environment and its explicitly shared Orb environment.
    /// The scope is a table selector, never a host context or authority token.
    fn selected_handles(&self, scope: Option<ScopeId>) -> Result<&Rc<Handles>> {
        let owner = scope.unwrap_or(self.id);
        if owner == self.id {
            return Ok(&self.handles);
        }
        self.parent
            .as_ref()
            .filter(|parent| parent.environment == owner)
            .ok_or_else(|| failure("foreign environment handle"))
    }
}

/// Typed hook handles keep composition in the ordinary native host.
#[derive(Clone)]
enum BoundHook {
    Series(tembra::HookHandle<Value, ()>, u32),
    Waterfall(tembra::HookHandle<Value, Value>, u32),
    Bail(tembra::HookHandle<Value, Option<Value>>, u32),
}
impl BoundHook {
    fn slot(&self) -> u32 {
        match self {
            Self::Series(_, slot) | Self::Waterfall(_, slot) | Self::Bail(_, slot) => *slot,
        }
    }
    async fn run(&self, context: &Context, value: Value) -> Result<Value> {
        match self {
            Self::Series(handle, _) => {
                handle.run(context, value).await?;
                Ok(Value::Null)
            }
            Self::Waterfall(handle, _) => handle.run(context, value).await,
            Self::Bail(handle, _) => Ok(handle.run(context, value).await?.unwrap_or(Value::Null)),
        }
    }
}
