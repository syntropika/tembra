use crate::engine::{Engine, invalid, limit};
use deno_core::v8;
use serde_json::Value;
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};
use tembra_core::*;

pub(crate) struct Module {
    pub(crate) persistent: RefCell<Vec<Rc<Capability>>>,
    pub(crate) host: u64,
    pub(crate) generation: u64,
    // Drop V8 handles/capabilities before the last engine owner.
    pub(crate) engine: Rc<Engine>,
}
pub(crate) struct FunctionSlot {
    pub(crate) function: v8::Global<v8::Function>,
    engine: Weak<Engine>,
    pub(crate) setup: Option<Rc<Owner>>,
}
impl Drop for FunctionSlot {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.upgrade() {
            engine.update(|c| c.callbacks -= 1);
        }
    }
}
pub(crate) struct Callback {
    pub(crate) function: v8::Global<v8::Function>,
    pub(crate) slot: Rc<FunctionSlot>,
    pub(crate) module: Rc<Module>,
}
impl Callback {
    pub(crate) fn new(module: &Rc<Module>, function: v8::Global<v8::Function>) -> Result<Rc<Self>> {
        Self::with_setup(module, function, None)
    }
    pub(crate) fn with_setup(
        module: &Rc<Module>,
        function: v8::Global<v8::Function>,
        setup: Option<Rc<Owner>>,
    ) -> Result<Rc<Self>> {
        if module.engine.counters.get().callbacks >= module.engine.options.max_callbacks {
            return Err(limit("callbacks"));
        }
        module.engine.update(|c| c.callbacks += 1);
        let slot = Rc::new(FunctionSlot {
            function: function.clone(),
            engine: Rc::downgrade(&module.engine),
            setup,
        });
        Ok(Rc::new(Self {
            function,
            slot,
            module: module.clone(),
        }))
    }
}
#[derive(Clone)]
pub(crate) struct JsValue {
    pub(crate) native: Option<Rc<dyn std::any::Any>>,
    pub(crate) setup: Option<Rc<Owner>>,
    pub(crate) value: v8::Global<v8::Value>,
    pub(crate) module: Rc<Module>,
}
impl JsValue {
    pub(crate) fn json(module: &Rc<Module>, value: Value) -> Result<Self> {
        let mut runtime = module
            .engine
            .runtime
            .try_borrow_mut()
            .map_err(|_| invalid("engine_busy", "value conversion requires idle engine"))?;
        deno_core::scope!(scope, runtime);
        let local = deno_core::serde_v8::to_v8(scope, value)
            .map_err(|e| invalid("javascript_value", e.to_string()))?;
        Ok(Self {
            value: v8::Global::new(scope, local),
            module: module.clone(),
            native: None,
            setup: None,
        })
    }
}

pub(crate) struct Owner {
    pub(crate) caps: RefCell<Vec<Rc<Capability>>>,
    pub(crate) module: Rc<Module>,
    pub(crate) setup: Option<Rc<Owner>>,
    pub(crate) invocation: Option<InvocationOwner>,
    pub(crate) selectors: RefCell<Vec<Rc<crate::binding::activity::SelectorBinding>>>,
}
impl Owner {
    pub(crate) fn context(&self) -> Result<Context> {
        self.caps
            .borrow()
            .iter()
            .find_map(|cap| {
                if let Kind::Context(context) = &cap.kind {
                    Some(context.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                invalid(
                    "invocation_context",
                    "callable requires an entered receiver context",
                )
            })
    }
    pub(crate) fn new(module: &Rc<Module>, invocation: Option<InvocationOwner>) -> Rc<Self> {
        Self::with_setup(module, invocation, None)
    }
    pub(crate) fn ancestry(self: &Rc<Self>) -> Rc<Self> {
        self.setup.clone().unwrap_or_else(|| self.clone())
    }
    pub(crate) fn child(parent: &Rc<Self>, invocation: Option<InvocationOwner>) -> Rc<Self> {
        Self::with_setup(&parent.module, invocation, Some(parent.ancestry()))
    }
    pub(crate) fn with_setup(
        module: &Rc<Module>,
        invocation: Option<InvocationOwner>,
        setup: Option<Rc<Self>>,
    ) -> Rc<Self> {
        Rc::new(Self {
            caps: RefCell::default(),
            module: module.clone(),
            invocation,
            setup,
            selectors: RefCell::default(),
        })
    }
}
#[derive(Clone)]
pub(crate) enum Signal {
    Core(CancellationToken),
    Facet(FacetSignal),
}
impl Signal {
    pub(crate) fn aborted(&self) -> bool {
        match self {
            Self::Core(token) => token.is_cancelled(),
            Self::Facet(signal) => signal.is_cancelled(),
        }
    }
    pub(crate) fn reason(&self) -> Option<RuntimeError> {
        match self {
            Self::Core(token) => token.check().err(),
            Self::Facet(signal) => {
                use futures::FutureExt;
                signal.cancelled().now_or_never()
            }
        }
    }
    pub(crate) fn cancelled(&self) -> futures::future::LocalBoxFuture<'static, ()> {
        use futures::FutureExt;
        match self {
            Self::Core(token) => {
                let token = token.clone();
                async move {
                    token.cancelled().await;
                }
                .boxed_local()
            }
            Self::Facet(signal) => {
                let signal = signal.clone();
                async move {
                    signal.cancelled().await;
                }
                .boxed_local()
            }
        }
    }
}
pub(crate) struct CallableSlot {
    pub(crate) callback: crate::native::NativeCallable,
    pub(crate) context: Context,
    pub(crate) invocation: InvocationOwner,
}
pub(crate) enum Kind {
    Scope(FacetScope),
    TaskScope(TaskScope),
    EventSink(Weak<crate::binding::activity::Delivery>),
    Setup(FacetRecorder),
    Declaration {
        recorder: DeclarationRecorder,
        owner: Weak<Owner>,
        principal: Value,
    },
    Context(Context),
    Signal(Signal),
    Reference(Reference),
    Cell(tembra_core::Cell),
    Readable(Readable),
    Resource(WeakResource<JsValue>),
    RegistryView(RegistryView),
    Operation(RegisteredOperation),
    Unsubscribe(RefCell<Option<Box<dyn Fn()>>>),
    StateSubscription(
        RefCell<Option<Subscription>>,
        RefCell<Option<Rc<FunctionSlot>>>,
    ),
    RegistrySubscription(
        RefCell<Option<RegistrySubscription>>,
        RefCell<Option<Rc<FunctionSlot>>>,
    ),
    Native(Rc<dyn std::any::Any>),
    Record(crate::native::NativeRecord),
    Callable(CallableSlot),
}
pub(crate) struct Capability {
    pub(crate) id: u64,
    pub(crate) kind: Kind,
    pub(crate) module: Weak<Module>,
    pub(crate) owner: Weak<Owner>,
    pub(crate) hook_manager: RefCell<Option<tembra_hooks::HookManager>>,
    context: bool,
}
impl Drop for Capability {
    fn drop(&mut self) {
        if self.context
            && let Some(module) = self.module.upgrade()
        {
            module.engine.update(|c| c.contexts -= 1);
        }
    }
}
impl Capability {
    pub(crate) fn new(
        module: &Rc<Module>,
        owner: Option<&Rc<Owner>>,
        kind: Kind,
    ) -> Result<Rc<Self>> {
        let engine = &module.engine;
        prune_operations(&module.persistent);
        if let Some(owner) = owner {
            prune_operations(&owner.caps);
        }
        let existing = if let Some(owner) = owner {
            owner
                .caps
                .borrow()
                .iter()
                .find(|cap| same_kind(&cap.kind, &kind))
                .cloned()
        } else {
            module
                .persistent
                .borrow()
                .iter()
                .find(|cap| same_kind(&cap.kind, &kind))
                .cloned()
        };
        if let Some(existing) = existing {
            return Ok(existing);
        }
        engine
            .capabilities
            .borrow_mut()
            .retain(|_, cap| cap.strong_count() > 0);
        if engine.capabilities.borrow().len() >= engine.options.max_capabilities {
            return Err(limit("capabilities"));
        }
        let context = matches!(kind, Kind::Context(_));
        if context && engine.counters.get().contexts >= engine.options.max_contexts {
            return Err(limit("contexts"));
        }
        let id = engine.next_capability.get();
        let next = id
            .checked_add(1)
            .filter(|id| *id <= 9_007_199_254_740_991)
            .ok_or_else(|| limit("capability_ids"))?;
        engine.next_capability.set(next);
        let cap = Rc::new(Self {
            id,
            kind,
            module: Rc::downgrade(module),
            owner: owner.map_or_else(Weak::new, Rc::downgrade),
            context,
            hook_manager: RefCell::default(),
        });
        engine
            .capabilities
            .borrow_mut()
            .insert(id, Rc::downgrade(&cap));
        if context {
            engine.update(|c| c.contexts += 1);
        }
        if let Some(owner) = owner {
            owner.caps.borrow_mut().push(cap.clone());
        } else {
            module.persistent.borrow_mut().push(cap.clone());
        }
        Ok(cap)
    }
    /// Ends this issued handle after explicit unsubscribe. Extract all user callbacks first.
    pub(crate) fn retire(&self) {
        let removed = if let Some(owner) = self.owner.upgrade() {
            let mut caps = owner.caps.borrow_mut();
            caps.iter()
                .position(|cap| cap.id == self.id)
                .map(|index| caps.remove(index))
        } else if let Some(module) = self.module.upgrade() {
            let mut caps = module.persistent.borrow_mut();
            caps.iter()
                .position(|cap| cap.id == self.id)
                .map(|index| caps.remove(index))
        } else {
            None
        };
        drop(removed);
    }
    pub(crate) fn context(&self) -> Result<Context> {
        match &self.kind {
            Kind::Context(ctx) => {
                let module = self.module.upgrade().ok_or_else(expired)?;
                ctx.check(module.host)?;
                Ok(ctx.clone())
            }
            _ => Err(invalid(
                "context_capability",
                "a real Context capability is required",
            )),
        }
    }
    pub(crate) fn owner(&self) -> Result<Rc<Owner>> {
        self.owner.upgrade().ok_or_else(expired)
    }
}
pub(crate) fn expired() -> RuntimeError {
    RuntimeError::aborted("expired", "capability owner ended")
}

fn same_kind(left: &Kind, right: &Kind) -> bool {
    match (left, right) {
        (Kind::Native(left), Kind::Native(right)) => Rc::ptr_eq(left, right),
        (Kind::Operation(left), Kind::Operation(right)) => left.same_target(right),
        _ => false,
    }
}

fn prune_operations(caps: &RefCell<Vec<Rc<Capability>>>) {
    let removed = {
        let mut caps = caps.borrow_mut();
        let indices = caps
            .iter()
            .enumerate()
            .filter_map(|(index, cap)| {
                matches!(&cap.kind,Kind::Operation(operation) if !operation.is_live_target())
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        indices
            .into_iter()
            .rev()
            .map(|index| caps.remove(index))
            .collect::<Vec<_>>()
    };
    drop(removed);
}
