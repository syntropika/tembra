//! Hook adapters retain callbacks; kernel plans select identities and retain physical owners.
use super::*;
use crate::hooks::{Contribution, Decode};
use std::any::TypeId;

type Dispatcher =
    Rc<dyn Fn(core::Id, Context, Value) -> LocalBoxFuture<'static, Result<Option<Value>>>>;

#[derive(Default)]
pub(super) struct Hooks {
    declarations: RefCell<BTreeMap<core::Id, BTreeMap<String, TypeId>>>,
    contributions: RefCell<BTreeMap<core::Id, (core::Id, Contribution)>>,
    dispatcher: RefCell<Option<Dispatcher>>,
}

impl Host {
    pub(super) fn validate_hooks(&self, orbs: &[Orb]) -> Result<()> {
        let current = self.kernel().installed_orbs();
        for orb in orbs {
            for contribution in &orb.contributions {
                if contribution.target != orb.metadata.name
                    && !orb.metadata.requires.iter().any(|requirement| {
                        requirement.orb == contribution.target
                            && requirement.hooks.contains(&contribution.hook)
                    })
                {
                    return Err(Error::new(
                        core::Code::Forbidden,
                        Reason::HookDependency,
                        "hook use must be declared in requires",
                    ));
                }
                let pending = orbs
                    .iter()
                    .find(|orb| orb.metadata.name == contribution.target);
                let installed = current
                    .iter()
                    .find(|orb| orb.declaration.name == contribution.target);
                let declarations = self.0.hooks.declarations.borrow();
                let signature = pending
                    .and_then(|orb| orb.hooks.get(&contribution.hook))
                    .or_else(|| {
                        installed
                            .and_then(|orb| declarations.get(&orb.id))
                            .and_then(|hooks| hooks.get(&contribution.hook))
                    });
                if signature.is_some_and(|signature| *signature != contribution.signature) {
                    return Err(type_error());
                }
                let exists = pending
                    .is_some_and(|orb| orb.metadata.hooks.contains(&contribution.hook))
                    || installed
                        .is_some_and(|orb| orb.declaration.hooks.contains(&contribution.hook));
                if !exists {
                    return Err(Error::invalid(Reason::Hook, &contribution.hook));
                }
            }
        }
        Ok(())
    }

    pub(super) fn install_hooks(
        &self,
        orb: core::Id,
        hooks: BTreeMap<String, TypeId>,
        contributions: Vec<Contribution>,
    ) -> Result<()> {
        self.0.hooks.declarations.borrow_mut().insert(orb, hooks);
        for contribution in contributions {
            let operation = self.kernel().allocate_handle()?;
            self.kernel().contribute(core::Contribution {
                orb,
                operation,
                target: contribution.target.clone(),
                hook: contribution.hook.clone(),
                priority: contribution.priority,
                ordinal: self.kernel().contribution_ordinal()?,
            })?;
            self.0
                .hooks
                .contributions
                .borrow_mut()
                .insert(operation, (orb, contribution));
        }
        Ok(())
    }

    /// Attach the foreign-language participant dispatcher for this shared host.
    /// The adapter must retain physical work and use the host's selected codec at its boundary.
    pub fn set_hook_dispatcher(
        &self,
        dispatch: impl Fn(core::Id, Context, Value) -> LocalBoxFuture<'static, Result<Option<Value>>>
        + 'static,
    ) {
        *self.0.hooks.dispatcher.borrow_mut() = Some(Rc::new(dispatch));
    }

    /// Check whether this adapter owns a contribution identity in a kernel plan.
    pub fn has_contribution(&self, operation: core::Id) -> bool {
        self.0.hooks.contributions.borrow().contains_key(&operation)
    }

    /// Execute a native contribution requested by a foreign adapter.
    /// `None` is an undefined result; `Some(Null)` remains a defined decision.
    pub async fn invoke_contribution(
        &self,
        operation: core::Id,
        root: core::Id,
        frame: core::Id,
        input: Value,
    ) -> Result<Option<Value>> {
        self.kernel().check_root(root)?;
        let (orb, contribution) = self
            .0
            .hooks
            .contributions
            .borrow()
            .get(&operation)
            .cloned()
            .ok_or_else(|| {
                Error::new(core::Code::Gone, Reason::Operation, "contribution is gone")
            })?;
        self.ready_orb(orb).await?;
        let call = self
            .kernel()
            .begin_operation_with_context(orb, root, operation, None, frame)?;
        let entered = self.context(call.root, Some(call.clone()))?;
        let receive = self.spawn_managed(call.id, async move {
            (contribution.portable)(entered, input).await
        })?;
        receive
            .await
            .map_err(|_| Error::invalid(Reason::Executor, "hook executor stopped"))?
    }

    pub(crate) async fn hook_participant<A: Serialize + 'static, R: 'static>(
        &self,
        operation: core::Id,
        context: &Context,
        input: A,
        decode: &Decode<R>,
    ) -> Result<R> {
        context.check()?;
        let contribution = self.0.hooks.contributions.borrow().get(&operation).cloned();
        if let Some((orb, contribution)) = contribution {
            self.ready_orb(orb).await?;
            context.check()?;
            if contribution.signature != TypeId::of::<(A, R)>() {
                return Err(type_error());
            }
            let call = self.kernel().begin_operation_with_context(
                orb,
                context.root,
                operation,
                None,
                context.frame(),
            )?;
            let _guard = InvocationGuard {
                host: self.clone(),
                call: call.id,
            };
            let entered = self.context(call.root, Some(call))?;
            let output = (contribution.local)(entered, Box::new(input)).await?;
            output
                .downcast::<R>()
                .map(|value| *value)
                .map_err(|_| type_error())
        } else {
            let dispatch = self.0.hooks.dispatcher.borrow().clone().ok_or_else(|| {
                Error::new(
                    core::Code::Gone,
                    Reason::Operation,
                    "no contribution adapter",
                )
            })?;
            decode(dispatch(operation, context.clone(), crate::hooks::to_value(input)?).await?)
        }
    }

    pub(super) fn clear_hooks(&self) {
        self.0.hooks.declarations.borrow_mut().clear();
        self.0.hooks.contributions.borrow_mut().clear();
        self.0.hooks.dispatcher.borrow_mut().take();
    }
}

struct InvocationGuard {
    host: Host,
    call: core::Id,
}
impl Drop for InvocationGuard {
    fn drop(&mut self) {
        if !self.host.0.external_actions.get() {
            self.host.tick();
        }
        if let Err(error) = self.host.kernel().finish_call(self.call) {
            self.host.0.errors.borrow_mut().push(error);
        }
        self.host.flush();
    }
}

impl Declaration {
    /// Bind one of this object's orb's declared hooks, checking its native contract.
    pub fn hook<A: 'static, R: 'static>(
        &self,
        hook: &crate::Hook<A, R>,
    ) -> Result<crate::HookHandle<A, R>> {
        self.check_declaration()?;
        let declarations = self.host.0.hooks.declarations.borrow();
        let signature = declarations
            .get(&self.orb)
            .and_then(|hooks| hooks.get(&hook.name))
            .ok_or_else(|| Error::invalid(Reason::Hook, &hook.name))?;
        if *signature != hook.signature() {
            return Err(type_error());
        }
        Ok(crate::HookHandle {
            host: self.host.clone(),
            orb: self.orb,
            target: self.host.kernel().orb(self.orb)?.declaration.name,
            object: if self.object == 0 {
                None
            } else {
                Some(self.object)
            },
            operation: self.host.kernel().allocate_handle()?,
            hook: hook.clone(),
        })
    }
}
