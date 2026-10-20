use crate::{
    callbacks::{Callback, NodeValue},
    factory::ObjectOwner,
};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};
use tembra_core::*;

pub(crate) struct Removal(pub RefCell<Option<Box<dyn Fn()>>>);
impl Removal {
    pub fn remove(&self) {
        let action = self.0.borrow_mut().take();
        if let Some(action) = action {
            action();
        }
    }
}
impl Drop for Removal {
    fn drop(&mut self) {
        self.remove();
    }
}

pub(crate) struct ResourceBuilder {
    pub owner: Weak<ObjectOwner>,
    pub open: Rc<Callback>,
    pub close: Option<Rc<Callback>>,
    pub version: u64,
    pub exclusive: bool,
    pub deps: Vec<ResourceDependency>,
}
pub(crate) struct Derivation {
    pub owner: Weak<ObjectOwner>,
    pub schema: serde_json::Value,
    pub compute: Rc<Callback>,
    pub sources: Vec<Readable>,
}
#[derive(Clone)]
pub(crate) struct IssuedContext {
    pub context: Context,
    pub invocation: Option<InvocationOwner>,
    pub signal: CancellationToken,
}
#[derive(Clone)]
pub(crate) enum Signal {
    Token(CancellationToken),
    Facet(FacetSignal),
}
impl Signal {
    pub fn aborted(&self) -> bool {
        match self {
            Self::Token(s) => s.is_cancelled(),
            Self::Facet(s) => s.is_cancelled(),
        }
    }
    pub fn cancelled(&self) -> futures::future::LocalBoxFuture<'static, RuntimeError> {
        match self {
            Self::Token(s) => s.cancelled(),
            Self::Facet(s) => s.cancelled(),
        }
    }
}
#[derive(Clone)]
pub(crate) enum Entry {
    Rpc(Rc<crate::ipc::RpcEndpoint>),
    Preparation(Rc<crate::loader::Preparation>),
    Generation(Rc<crate::loader::Generation>),
    Candidate(Rc<RefCell<Option<ExecutableFacet>>>),
    Reference(Reference),
    State(Readable),
    Cell(Cell),
    Context(IssuedContext),
    Signal(Signal),
    Scope(FacetScope),
    Recorder(FacetRecorder),
    Object(Weak<ObjectOwner>),
    Resource(WeakResource<NodeValue>),
    ResourceBuilder(Rc<RefCell<Option<ResourceBuilder>>>),
    Derivation(Rc<RefCell<Option<Derivation>>>),
    Subscription(Rc<Removal>),
    Registry(RegistryView),
    Table(RegistrySnapshot),
    Operation(RegisteredOperation),
    Task(TaskScope),
    Sink(Weak<crate::activity::Delivery>),
    Continuation(Rc<dyn crate::hooks::Continuation>),
    Capability {
        resource: WeakResource<NodeValue>,
        nominal: String,
    },
}
impl Entry {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Rpc(_) => "rpc",
            Self::Preparation(_) => "preparation",
            Self::Generation(_) => "generation",
            Self::Candidate(_) => "candidate",

            Self::Reference(_) => "reference",
            Self::State(_) => "state",
            Self::Cell(_) => "cell",
            Self::Context(_) => "context",
            Self::Signal(_) => "signal",
            Self::Scope(_) => "scope",
            Self::Recorder(_) => "recorder",
            Self::Object(_) => "object",
            Self::Resource(_) => "resource",
            Self::ResourceBuilder(_) => "resourceBuilder",
            Self::Derivation(_) => "derivation",
            Self::Subscription(_) => "subscription",
            Self::Registry(_) => "registry",
            Self::Table(_) => "table",
            Self::Operation(_) => "operation",
            Self::Task(_) => "task",
            Self::Sink(_) => "sink",
            Self::Continuation(_) => "continuation",
            Self::Capability { .. } => "capability",
        }
    }
}
