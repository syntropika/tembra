//! One experimental installation path for native and component policy contributions.
//!
//! Tembra selects plans and versions. This table stores callback values only.
//! Contracts and domain validation belong to the consumer crate.

use futures::{FutureExt, future::LocalBoxFuture};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra::{Code, Context, Error, Host, Result, core};

type Policy<I, O> = dyn Fn(Context, I) -> LocalBoxFuture<'static, Result<O>>;

pub struct PreparedGuard<I, O> {
    execute: Box<Policy<I, O>>,
    requirements: Vec<core::Requirement>,
    dropped: Option<Rc<Cell<usize>>>,
}

impl<I: 'static, O: 'static> PreparedGuard<I, O> {
    pub fn from_async(
        policy: impl Fn(Context, I) -> LocalBoxFuture<'static, Result<O>> + 'static,
    ) -> Self {
        Self {
            execute: Box::new(policy),
            requirements: Vec::new(),
            dropped: None,
        }
    }

    pub fn requires(mut self, requirement: core::Requirement) -> Self {
        self.requirements.push(requirement);
        self
    }
}

impl<I, O> Drop for PreparedGuard<I, O> {
    fn drop(&mut self) {
        if let Some(dropped) = &self.dropped {
            dropped.set(dropped.get() + 1);
        }
    }
}

struct Entry<I, O> {
    orb: core::Id,
    guard: Rc<PreparedGuard<I, O>>,
}

struct State<I, O> {
    host: Host,
    target: String,
    hook: String,
    entries: RefCell<BTreeMap<core::Id, Entry<I, O>>>,
    installed: Cell<usize>,
    dropped: Rc<Cell<usize>>,
    cleanup_errors: RefCell<Vec<Error>>,
    observations: RefCell<Vec<Value>>,
}

impl<I: DeserializeOwned + 'static, O: Serialize + 'static> State<I, O> {
    fn collect(&self) {
        let kernel = self.host.kernel();
        self.entries
            .borrow_mut()
            .retain(|_, entry| kernel.orb(entry.orb).is_ok());
    }

    async fn dispatch(
        self: &Rc<Self>,
        operation: core::Id,
        context: Context,
        input: Value,
    ) -> Result<Value> {
        context.check()?;
        let (orb, guard) = self
            .entries
            .borrow()
            .get(&operation)
            .map(|entry| (entry.orb, entry.guard.clone()))
            .ok_or_else(|| {
                Error::new(Code::Gone, "guard.operation", "guard is no longer retained")
            })?;
        let input: I = serde_json::from_value(input)
            .map_err(|error| Error::invalid("guard.input", error.to_string()))?;
        let call = self.host.kernel().begin_operation_with_context(
            orb,
            context.root,
            operation,
            None,
            context.frame(),
        )?;
        let _settle = Settle {
            state: self.clone(),
            call: call.id,
        };
        let entered = self.host.context(call.root, Some(call.clone()))?;
        self.observations.borrow_mut().push(serde_json::json!({
            "root": call.root, "invocation": call.id, "orb": orb,
            "version": call.code_version, "principal": entered.principal()?,
        }));
        let output = (guard.execute)(entered, input).await?;
        serde_json::to_value(output)
            .map_err(|error| Error::invalid("guard.output", error.to_string()))
    }
}

struct Settle<I: DeserializeOwned + 'static, O: Serialize + 'static> {
    state: Rc<State<I, O>>,
    call: core::Id,
}

impl<I: DeserializeOwned + 'static, O: Serialize + 'static> Drop for Settle<I, O> {
    fn drop(&mut self) {
        if let Err(error) = self.state.host.kernel().finish_call(self.call) {
            self.state.cleanup_errors.borrow_mut().push(error);
        }
        self.state.host.flush();
        self.state.collect();
    }
}

pub struct Runtime<I, O> {
    state: Rc<State<I, O>>,
}

impl<I: DeserializeOwned + 'static, O: Serialize + 'static> Runtime<I, O> {
    /// Attach as the sole foreign-hook value adapter on an existing native host.
    pub fn attach(host: Host, target: &str, hook: &str) -> Result<Self> {
        let provider = host.kernel().orb_named(target)?;
        if !provider.declaration.hooks.contains(hook) {
            return Err(Error::invalid(
                "guard.hook",
                "the target hook is not declared",
            ));
        }
        let state = Rc::new(State {
            host: host.clone(),
            target: target.into(),
            hook: hook.into(),
            entries: RefCell::default(),
            installed: Cell::new(0),
            dropped: Rc::new(Cell::new(0)),
            cleanup_errors: RefCell::default(),
            observations: RefCell::default(),
        });
        let weak = Rc::downgrade(&state);
        host.set_hook_dispatcher(move |operation, context, input| {
            let state = weak.upgrade();
            async move {
                let state = state.ok_or_else(|| {
                    Error::new(Code::Gone, "guard.runtime", "adapter was dropped")
                })?;
                state.dispatch(operation, context, input).await.map(Some)
            }
            .boxed_local()
        });
        let weak = Rc::downgrade(&state);
        host.set_foreign_notifier(move |_| {
            if let Some(state) = weak.upgrade() {
                state.collect();
            }
        });
        Ok(Self { state })
    }

    /// The same adoption path accepts either kind of prepared policy.
    pub fn install(&self, id: &str, mut guard: PreparedGuard<I, O>) -> Result<core::Version> {
        let kernel = self.state.host.kernel();
        let provider = kernel.orb_named(&self.state.target)?;
        let declaration = core::Orb::new(id, 1);
        let mut requirements = guard.requirements.clone();
        requirements.push(core::Requirement {
            orb: self.state.target.clone(),
            api_version: provider.declaration.api_version,
            methods: Default::default(),
            hooks: [self.state.hook.clone()].into(),
        });
        let declaration = core::Orb {
            requires: requirements,
            ..declaration
        };
        let operation = kernel.allocate_handle()?;
        let ordinal = kernel.contribution_ordinal()?;
        let candidate = kernel.prepare(vec![declaration], Vec::new())?;
        let Some(orb) = candidate.orbs.first().map(|orb| orb.id) else {
            kernel.abort(candidate.id)?;
            return Err(Error::invalid(
                "guard.candidate",
                "candidate is missing its implementation",
            ));
        };
        let version = match kernel.commit(candidate.id, Vec::new()) {
            Ok(version) => version,
            Err(error) => {
                kernel.abort(candidate.id)?;
                return Err(error);
            }
        };
        // This synchronous registration cannot interleave with another local root.
        // A post-commit refusal is reported explicitly; no rollback is claimed.
        if let Err(error) = kernel.contribute(core::Contribution {
            orb,
            target: self.state.target.clone(),
            hook: self.state.hook.clone(),
            operation,
            priority: 0,
            ordinal,
        }) {
            return Err(Error::new(
                Code::Unavailable,
                "guard.adoption_unconfirmed",
                format!(
                    "version {version} committed but contribution registration failed: {error}"
                ),
            ));
        }
        guard.dropped = Some(self.state.dropped.clone());
        self.state.installed.set(self.state.installed.get() + 1);
        self.state.entries.borrow_mut().insert(
            operation,
            Entry {
                orb,
                guard: Rc::new(guard),
            },
        );
        self.state.host.flush();
        self.state.collect();
        Ok(version)
    }

    pub fn status(&self) -> Value {
        serde_json::json!({
            "retained_callbacks": self.state.entries.borrow().len(),
            "installed": self.state.installed.get(), "dropped": self.state.dropped.get(),
            "observations": *self.state.observations.borrow(),
            "cleanup_errors": self.state.cleanup_errors.borrow().iter().map(ToString::to_string).collect::<Vec<_>>(),
        })
    }

    pub async fn close(&self) -> Result<()> {
        self.state.host.close().await?;
        self.state.collect();
        if let Some(error) = self.state.cleanup_errors.borrow_mut().pop() {
            return Err(error);
        }
        Ok(())
    }
}

#[path = "adapter/imports.rs"]
pub mod imports;
