//! Bind guest operations to issued declarations and contexts on the native host thread.
use crate::{
    failure,
    protocol::{GuestCommand, ScopeId},
    worker::Session,
};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra::{
    Cancellation, Context, Declaration, Reference, Resource, ResourceSignal, Result, State,
};

mod operations;

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
}
pub(crate) struct BoundMethod {
    pub name: String,
    pub slot: u32,
}

#[derive(Clone)]
struct ReferenceHandle {
    reference: Rc<Reference>,
    import: Rc<Import>,
}

struct Handles {
    session: Session,
    environment: u64,
    next: Cell<u32>,
    states: RefCell<BTreeMap<u32, State<Value>>>,
    references: RefCell<BTreeMap<u32, ReferenceHandle>>,
    resources: RefCell<BTreeMap<u32, Resource<u32>>>,
}

impl Drop for Handles {
    /// Release guest closures only after the last physical native owner has settled.
    fn drop(&mut self) {
        self.session.release_environment(self.environment);
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
    ) -> Result<Self> {
        let parent = if service.is_some() {
            Some(declaration.orb::<Environment>()?.handles.clone())
        } else {
            None
        };
        let handles = Rc::new(Handles {
            session: session.clone(),
            environment: id,
            next: Cell::new(0),
            states: RefCell::default(),
            references: RefCell::default(),
            resources: RefCell::default(),
        });
        let environment = Self {
            session,
            id,
            handles,
            service,
            imports,
            parent,
            contract,
        };
        let scope = Scope::Declaration(declaration);
        environment.session.blocking(
            crate::abi::Operation::Declare,
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
        };
        self.execute(Scope::Call(context), Some(cancellation), packet)
            .await
    }

    /// Run a callback while routing operations through this environment's owned handle table.
    /// A cancellable callback may leave the admission queue; cleanup passes no cancellation.
    async fn execute(
        &self,
        scope: Scope,
        cancellation: Option<Cancellation>,
        packet: GuestCommand<'_>,
    ) -> Result<Value> {
        let admission = self.session.admit(cancellation.as_ref()).await?;
        if cancellation.is_some() {
            // Authority can expire while the callback waits behind another one.
            Self::authorize(&scope)?;
        }
        self.session
            .dispatch(admission, packet, async |packet, asynchronous| {
                if asynchronous {
                    self.asynchronous(&scope, packet).await
                } else {
                    self.sync(&scope, packet)
                }
            })
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
