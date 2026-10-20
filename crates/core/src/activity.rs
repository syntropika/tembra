//! Managed tasks and bounded, identity-bearing event listeners.
//!
//! Cancellation revokes new roots but does not prove physical callback completion.
use crate::{
    CancellationToken, Context, Declaration, DeclarationRecorder, FacetSignal, Readable, Resource,
    Result, RuntimeError, Snapshot, host::HostInner, object::ObjectRecord,
};
use futures::future::{FutureExt, LocalBoxFuture};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
};

mod declaration;
mod runtime;
pub(crate) use runtime::{
    ActivityActions, ActivityFailureCheckpoint, ActivityRunSet, PreparedActivities,
};

type Detach = Box<dyn FnOnce()>;
type Subscribe<E> = dyn Fn(Rc<dyn Fn(E)>) -> Result<Detach>;
/// An application-owned typed source. Clones preserve exact subscription identity.
/// Event payload size is application-owned; the runtime bounds pending event count.
pub struct EventSource<E: 'static> {
    subscribe: Rc<Subscribe<E>>,
    owners: Rc<Vec<Rc<dyn Any>>>,
}
impl<E: 'static> Clone for EventSource<E> {
    fn clone(&self) -> Self {
        Self {
            subscribe: self.subscribe.clone(),
            owners: self.owners.clone(),
        }
    }
}
impl<E: 'static> EventSource<E> {
    /// Stores a trusted subscribe callback without subscribing yet.
    /// A successful subscription returns an idempotently owned one-shot detach action.
    pub fn new(subscribe: impl Fn(Rc<dyn Fn(E)>) -> Result<Box<dyn FnOnce()>> + 'static) -> Self {
        Self {
            subscribe: Rc::new(subscribe),
            owners: Rc::new(Vec::new()),
        }
    }
    /// Observes this exact identity without retaining subscription captures.
    pub fn downgrade(&self) -> WeakEventSource<E> {
        WeakEventSource {
            subscribe: Rc::downgrade(&self.subscribe),
            owners: Rc::downgrade(&self.owners),
        }
    }
    /// Keeps an embedding callback owner alive through this clone's actual declaration lifetime.
    /// Other clones retain their own anchors; the underlying source identity is unchanged.
    pub fn retain_owner<T: 'static>(mut self, owner: Rc<T>) -> Self {
        let mut owners = self.owners.as_ref().clone();
        owners.push(owner);
        self.owners = Rc::new(owners);
        self
    }
    pub(crate) fn same_source(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.subscribe, &other.subscribe)
    }
}
/// A weak observation of an existing event source identity.
pub struct WeakEventSource<E: 'static> {
    subscribe: Weak<Subscribe<E>>,
    owners: Weak<Vec<Rc<dyn Any>>>,
}
impl<E: 'static> Clone for WeakEventSource<E> {
    fn clone(&self) -> Self {
        Self {
            subscribe: self.subscribe.clone(),
            owners: self.owners.clone(),
        }
    }
}
impl<E: 'static> WeakEventSource<E> {
    /// Returns the same source while a declaration or caller still owns it.
    pub fn upgrade(&self) -> Option<EventSource<E>> {
        Some(EventSource {
            subscribe: self.subscribe.upgrade()?,
            owners: self.owners.upgrade()?,
        })
    }
}
/// Checked queue cardinality, independent of arbitrary native payload size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenerOptions {
    max_pending: usize,
}
impl Default for ListenerOptions {
    fn default() -> Self {
        Self { max_pending: 64 }
    }
}
impl ListenerOptions {
    /// Selects a positive bounded pending-event count without subscribing to any source.
    ///
    /// # Errors
    /// Refuses zero and values above 1,024 before any subscription or handler executes.
    ///
    /// ```
    /// use tembra_core::ListenerOptions;
    ///
    /// let installed = ListenerOptions::new(8)?;
    /// assert_eq!(installed.max_pending(), 8);
    /// assert_eq!(ListenerOptions::new(0).unwrap_err().reason, "listener_limit");
    /// assert_eq!(installed.max_pending(), 8);
    /// assert_eq!(ListenerOptions::default().max_pending(), 64);
    /// # Ok::<(), tembra_core::RuntimeError>(())
    /// ```
    pub fn new(max_pending: usize) -> Result<Self> {
        if !(1..=1024).contains(&max_pending) {
            return Err(RuntimeError::invalid(
                "listener_limit",
                "pending events must be between 1 and 1024",
            ));
        }
        Ok(Self { max_pending })
    }
    pub fn max_pending(&self) -> usize {
        self.max_pending
    }
}

pub(crate) struct ActivityAdmission {
    active: Cell<bool>,
    signal: CancellationToken,
    entry: RefCell<Weak<crate::invocation::InvocationEntry>>,
}
impl ActivityAdmission {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            active: Cell::new(true),
            signal: CancellationToken::new(),
            entry: RefCell::default(),
        })
    }
    pub(crate) fn check(&self) -> Result<()> {
        if !self.active.get() {
            return Err(RuntimeError::aborted(
                "activity_stopped",
                "activity cannot start new roots",
            ));
        }
        self.signal.check()
    }
    pub(crate) fn signal(&self) -> CancellationToken {
        self.signal.clone()
    }
    pub(crate) fn set_entry(&self, entry: Weak<crate::invocation::InvocationEntry>) {
        *self.entry.borrow_mut() = entry;
    }
    pub(crate) fn stop(&self, cause: RuntimeError) {
        self.active.set(false);
        self.signal.cancel(cause);
    }
}
/// A weak task authority. Every root selects the Host's actual current code epoch.
#[derive(Clone)]
pub struct TaskScope {
    host: Weak<HostInner>,
    object: Weak<ObjectRecord>,
    admission: Weak<ActivityAdmission>,
    declarations: Weak<crate::host::reload::DeclarationSet>,
}
impl TaskScope {
    /// Retains this admitted task's physical entry for independently driven backend work.
    /// This lease extends neither root authority nor callable code-version admission.
    /// # Errors
    /// Refuses stopped tasks, closed Hosts, replaced declarations and ended physical entries.
    pub fn retain(&self) -> Result<crate::InvocationLease> {
        let admission = self
            .admission
            .upgrade()
            .ok_or_else(|| RuntimeError::aborted("activity_stopped", "activity ended"))?;
        admission.check()?;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
        if host.is_closing() {
            return Err(RuntimeError::unavailable("closed", "Host is closing"));
        }
        host.epochs.check()?;
        let owner = self
            .object
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("activity_owner", "task owner ended"))?;
        let declarations = self
            .declarations
            .upgrade()
            .ok_or_else(|| RuntimeError::aborted("activity_stopped", "declarations retired"))?;
        let current = owner
            .installed
            .borrow()
            .get(&owner.current_epoch.get())
            .cloned();
        if !current.is_some_and(|installed| Rc::ptr_eq(&installed.declarations, &declarations)) {
            return Err(RuntimeError::aborted(
                "activity_stopped",
                "task declarations were replaced",
            ));
        }
        let entry = admission.entry.borrow().clone();
        crate::invocation::retain_task_entry(&host, &owner, &entry)
    }
    pub fn signal(&self) -> FacetSignal {
        FacetSignal::new(
            self.admission
                .upgrade()
                .map(|run| run.signal())
                .unwrap_or_else(|| {
                    let token = CancellationToken::new();
                    token.cancel(RuntimeError::aborted("activity_stopped", "activity ended"));
                    token
                }),
        )
    }
    /// Starts an actual fresh root; retaining this scope cannot revive a stopped task.
    /// # Errors
    /// Refuses expired activity, unavailable Host/owner or ordinary Host epoch admission.
    /// Dropping the result waiter expires its logical root, not independently admitted jobs.
    pub fn root<R: 'static>(
        &self,
        body: impl FnOnce(Context) -> LocalBoxFuture<'static, Result<R>> + 'static,
    ) -> LocalBoxFuture<'static, Result<R>> {
        let admitted = self.retain().and_then(|lease| {
            let host = self
                .host
                .upgrade()
                .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?;
            let owner = self
                .object
                .upgrade()
                .ok_or_else(|| RuntimeError::unavailable("activity_owner", "task owner ended"))?;
            let active = self
                .admission
                .upgrade()
                .ok_or_else(|| RuntimeError::aborted("activity_stopped", "task ended"))?;
            let (context, guard) = host.activity_root(&owner, &active)?;
            Ok((lease, context, guard))
        });
        async move {
            let (lease, context, guard) = admitted?;
            let result = body(context).await;
            drop(guard);
            drop(lease);
            result
        }
        .boxed_local()
    }
}
type Task = Rc<dyn Fn(TaskScope) -> LocalBoxFuture<'static, Result<()>>>;
type Handler = Rc<dyn Fn(Box<dyn Any>, Context) -> LocalBoxFuture<'static, Result<()>>>;
type SourceMatch = Rc<dyn Fn(&dyn Any) -> bool>;
type ErasedSubscribe = Rc<
    dyn Fn(Rc<dyn Fn(Box<dyn Any>)>, Option<&crate::state::GraphDeclarationSet>) -> Result<Detach>,
>;
struct ListenerDeclaration {
    identity: Rc<dyn Any>,
    same: SourceMatch,
    subscribe: ErasedSubscribe,
    handler: Handler,
    options: ListenerOptions,
    state: bool,
}
/// One exact declaration generation; journals do not retain a transaction coordinator.
pub(crate) struct ActivityDeclarations {
    listeners: RefCell<BTreeMap<String, Rc<ListenerDeclaration>>>,
    tasks: RefCell<Vec<Task>>,
}
