//! Bind guest operations to issued declarations and contexts on the native host thread.
use crate::{failure, worker::Session};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra::{Context, Declaration, Reference, Resource, ResourceSignal, Result, State};

mod operations;

#[derive(Clone)]
pub(crate) enum Scope {
    Declaration(Declaration),
    Call(Context),
    Open(ResourceSignal),
    Validation,
}

struct Handles {
    session: Session,
    environment: u64,
    realm: tembra::core::Id,
    next: Cell<u32>,
    states: RefCell<BTreeMap<u32, State<Value>>>,
    references: RefCell<BTreeMap<u32, (Rc<Reference>, String)>>,
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
    imports: Rc<BTreeMap<String, String>>,
    parent: Option<Rc<Handles>>,
    contract: Rc<tembra_contracts::Contract>,
}

impl Environment {
    /// Build one isolated environment using a host-issued declaration capability.
    pub(crate) fn create(
        session: Session,
        id: u64,
        declaration: Declaration,
        service: Option<String>,
        imports: Rc<BTreeMap<String, String>>,
        contract: Rc<tembra_contracts::Contract>,
    ) -> Result<Self> {
        let parent = if service.is_some() {
            Some(declaration.orb::<Environment>()?.handles.clone())
        } else {
            None
        };
        let realm = match &parent {
            Some(parent) => parent.realm,
            None => declaration.resource_realm()?,
        };
        let handles = Rc::new(Handles {
            session: session.clone(),
            environment: id,
            realm,
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
            "declare",
            json!({"kind":"create","environment":id,"service":environment.service}).to_string(),
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
        context.check()?;
        const STACK: tembra::core::ContextKey<String> =
            tembra::core::ContextKey::new("tembra.executor.wasm.stack", 1);
        let stack = context.value(&STACK)?.unwrap_or_default();
        let current = self.session.identity().to_string();
        if stack.split(',').any(|identity| identity == current) {
            return Err(failure(
                "causal reentry into the same interpreter is not supported",
            ));
        }
        let context = context.with(&STACK, format!("{stack},{current}"))?;
        let packet = json!({"kind":"invoke","environment":self.id,"service":self.service,"method":method,"args":args});
        self.execute(Scope::Call(context), packet).await
    }

    /// Run a callback while routing operations through this environment's owned handle table.
    async fn execute(&self, scope: Scope, packet: Value) -> Result<Value> {
        self.session
            .dispatch(packet, async |packet, asynchronous| {
                if asynchronous {
                    self.asynchronous(&scope, packet).await
                } else {
                    self.sync(&scope, packet)
                }
            })
            .await
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
    fn selected_handles(&self, request: &Value) -> Result<&Rc<Handles>> {
        let owner = request
            .get("scope")
            .map(|value| {
                value
                    .as_u64()
                    .ok_or_else(|| failure("invalid scope handle"))
            })
            .transpose()?
            .unwrap_or(self.id);
        if owner == self.id {
            return Ok(&self.handles);
        }
        self.parent
            .as_ref()
            .filter(|parent| parent.environment == owner)
            .ok_or_else(|| failure("foreign environment handle"))
    }
}
