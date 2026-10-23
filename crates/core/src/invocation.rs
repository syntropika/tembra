//! Checked physical retention of work admitted by an existing local method call.

use crate::{Context, ErrorCode, Implementation, Result, RuntimeError, ScopeGuard};
use crate::{
    host::HostInner,
    object::{ObjectRecord, Phase},
};
use futures::{FutureExt, future::LocalBoxFuture};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context as TaskContext, Poll},
};

/// Observation of the final physical entry release. It grants no call authority.
#[derive(Clone)]
pub struct InvocationReceipt {
    pub(crate) state: Rc<ReceiptState>,
}
pub(crate) struct ReceiptState {
    entered: Cell<bool>,
    entry: RefCell<Weak<InvocationEntry>>,
    settled: crate::CancellationToken,
}
impl InvocationReceipt {
    pub(crate) fn new() -> Self {
        Self {
            state: Rc::new(ReceiptState {
                entered: Cell::new(false),
                entry: RefCell::default(),
                settled: crate::CancellationToken::new(),
            }),
        }
    }
    /// Waits independently of caller cancellation, without retaining the entry.
    /// The entry releases its selected implementation pin and updates physical ownership
    /// before notifying these waiters. Other owners may still retain the implementation.
    pub fn settled(&self) -> LocalBoxFuture<'static, ()> {
        let token = self.state.settled.clone();
        async move {
            token.cancelled().await;
        }
        .boxed_local()
    }
    pub(crate) fn is_settled(&self) -> bool {
        self.state.settled.is_cancelled()
    }
    /// Tests exact entered receiver scope identity; this never bypasses Context checks.
    pub fn matches_context(&self, context: &Context) -> bool {
        self.state.entry.borrow().upgrade().is_some_and(|entry| {
            entry
                .context
                .as_ref()
                .is_some_and(|entered| entered.same_scope(context))
        })
    }
    pub(crate) fn finish_unentered(&self) {
        if !self.state.entered.get() {
            self.state.finish();
        }
    }
}
impl ReceiptState {
    fn finish(&self) {
        self.settled.cancel(RuntimeError::aborted(
            "settled",
            "tracked physical entry settled",
        ));
    }
}

/// An opt-in result future with a separately observable physical completion receipt.
/// Dropping an unpolled call enters no handler and completes its receipt. After entry,
/// detached InvocationLeases can keep that receipt pending after this result or waiter ends.
///
/// # Panics
/// Polling this future after it returned `Ready` panics. Repeated polling is not a new call.
#[must_use = "poll the call to invoke it; retain its receipt to observe physical settlement"]
pub struct ManagedCall<R> {
    future: Option<LocalBoxFuture<'static, Result<R>>>,
    receipt: InvocationReceipt,
}
impl<R> ManagedCall<R> {
    pub(crate) fn new(
        receipt: InvocationReceipt,
        future: LocalBoxFuture<'static, Result<R>>,
    ) -> Self {
        Self {
            future: Some(future),
            receipt,
        }
    }
    /// Observes final entry release independently of result delivery or caller cancellation.
    /// This grants no authority and does not retain an entered invocation.
    pub fn receipt(&self) -> InvocationReceipt {
        self.receipt.clone()
    }
}
impl<R> Future for ManagedCall<R> {
    type Output = Result<R>;
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this
            .future
            .as_mut()
            .expect("tracked call polled after completion")
            .as_mut()
            .poll(cx)
        {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                drop(this.future.take());
                this.receipt.finish_unentered();
                Poll::Ready(result)
            }
        }
    }
}
impl<R> Drop for ManagedCall<R> {
    fn drop(&mut self) {
        drop(self.future.take());
        self.receipt.finish_unentered();
    }
}

/// Weak receiver authority minted by a real object declaration.
///
/// This does not keep its object alive or invoke a method. Each retention requires the
/// exact receiver Context of an already entered call, rather than a root or descendant.
#[derive(Clone)]
pub struct InvocationOwner {
    host: Weak<HostInner>,
    object: Weak<ObjectRecord>,
}

/// Retains one already admitted invocation until independently owned work settles.
///
/// Dropping the logical caller expires its Context even while this lease exists. Keep the
/// lease through physical callback completion and result extraction; cancellation alone
/// does not prove that resource-using work stopped. Drop outside engine/state borrows:
/// final release can wake cleanup. This type deliberately does not implement Clone.
///
/// ```compile_fail
/// use tembra_core::InvocationLease;
/// fn duplicate(lease: InvocationLease) -> InvocationLease { lease.clone() }
/// ```
#[must_use = "retain the lease until the admitted work physically settles"]
pub struct InvocationLease {
    _entry: Rc<InvocationEntry>,
}

impl InvocationLease {
    pub(crate) fn activity_receipt(&self) -> Result<InvocationReceipt> {
        if self._entry.context.is_some() {
            return Err(RuntimeError::invalid(
                "activity_owner",
                "entry is an entered method",
            ));
        }
        self._entry
            .receipt
            .as_ref()
            .map(|state| InvocationReceipt {
                state: state.clone(),
            })
            .ok_or_else(|| RuntimeError::invalid("activity_owner", "task entry has no receipt"))
    }
    pub(crate) fn attach_activity(&self, admission: &crate::activity::ActivityAdmission) {
        admission.set_entry(Rc::downgrade(&self._entry));
    }
}
pub(crate) fn retain_task_entry(
    host: &Rc<HostInner>,
    object: &Rc<ObjectRecord>,
    entry: &Weak<InvocationEntry>,
) -> Result<InvocationLease> {
    host.epochs.check()?;
    let entry = entry
        .upgrade()
        .ok_or_else(|| RuntimeError::aborted("activity_stopped", "task entry settled"))?;
    if !Rc::ptr_eq(&entry.object, object)
        || !Weak::ptr_eq(&entry.host, &Rc::downgrade(host))
        || object.phase.get() != Phase::Live
        || entry.context.is_some()
    {
        return Err(RuntimeError::invalid(
            "activity_owner",
            "task entry belongs to another or ended owner",
        ));
    }
    Ok(InvocationLease { _entry: entry })
}

impl InvocationOwner {
    pub(crate) fn new(host: &Rc<HostInner>, object: &Rc<ObjectRecord>) -> Self {
        Self {
            host: Rc::downgrade(host),
            object: Rc::downgrade(object),
        }
    }

    /// Retains physical ownership without extending logical Context admission.
    ///
    /// # Errors
    /// Returns `unavailable/closed` for a dropped Host, then existing Context failures;
    /// `invalid/invocation_context` for a root,
    /// descendant or ended entry; `invalid/invocation_owner` for another receiver;
    /// `gone/invocation_owner` for ended owner admission; and `superseded/code_version`
    /// for an unavailable version. Refusal issues no lease and changes no invocation count.
    pub fn retain(&self, context: &Context) -> Result<InvocationLease> {
        let host = self.host.upgrade().ok_or_else(|| {
            RuntimeError::unavailable("closed", "invocation Host no longer exists")
        })?;
        context.check(host.id)?;
        let entry = context
            .invocation()
            .filter(|entry| {
                entry
                    .context
                    .as_ref()
                    .is_some_and(|entered| entered.same_scope(context))
            })
            .ok_or_else(|| {
                RuntimeError::invalid(
                    "invocation_context",
                    "context is not an entered receiver scope",
                )
            })?;
        if !Weak::ptr_eq(&self.object, &Rc::downgrade(&entry.object)) {
            return Err(RuntimeError::invalid(
                "invocation_owner",
                "context entered another receiver",
            ));
        }
        if entry.object.phase.get() != Phase::Live {
            return Err(RuntimeError::new(
                ErrorCode::Gone,
                "invocation_owner",
                "receiver no longer admits work",
            ));
        }
        host.epochs.exact(entry.version)?;
        if context.code_version() != entry.version
            || entry.version < entry.object.min_code_version.get()
        {
            return Err(RuntimeError::new(
                ErrorCode::Superseded,
                "code_version",
                "invocation version is unavailable",
            ));
        }
        Ok(InvocationLease { _entry: entry })
    }
}

pub(crate) struct ActiveInvocation {
    pub(crate) version: u64,
    pub(crate) cancellation: crate::CancellationToken,
}

pub(crate) struct InvocationEntry {
    observation: Option<crate::observability::Span>,
    host: Weak<HostInner>,
    object: Rc<ObjectRecord>,
    implementation: Option<Rc<Implementation>>,
    receipt: Option<Rc<ReceiptState>>,
    context: Option<Context>,
    installed: Option<Rc<crate::host::reload::InstalledVersion>>,
    version: u64,
    id: u64,
}

pub(crate) struct LogicalInvocationGuard {
    scope: Option<ScopeGuard>,
    entry: Option<Rc<InvocationEntry>>,
}
impl Drop for LogicalInvocationGuard {
    fn drop(&mut self) {
        // Scope hooks and final-entry idle callbacks must never observe a live caller.
        drop(self.scope.take());
        drop(self.entry.take());
    }
}

pub(crate) fn enter(
    host: &Rc<HostInner>,
    object: Rc<ObjectRecord>,
    implementation: Rc<Implementation>,
    parent: &Context,
) -> Result<(Context, LogicalInvocationGuard)> {
    enter_with_receipt(host, object, implementation, parent, None)
}

pub(crate) fn enter_with_receipt(
    host: &Rc<HostInner>,
    object: Rc<ObjectRecord>,
    implementation: Rc<Implementation>,
    parent: &Context,
    receipt: Option<&InvocationReceipt>,
) -> Result<(Context, LogicalInvocationGuard)> {
    host.epochs.exact(parent.code_version())?;
    host.roots.touch(parent, &object)?;
    let limit = || RuntimeError::unavailable("invocation_limit", "invocation counter exhausted");
    let id = object
        .next_invocation
        .get()
        .checked_add(1)
        .ok_or_else(limit)?;
    let count = object.invocations.get().checked_add(1).ok_or_else(limit)?;
    let installed = object
        .installed
        .borrow()
        .get(&parent.code_version())
        .cloned();
    let facet = installed
        .as_ref()
        .and_then(|installed| installed.service.owner_name.clone());
    let (child, scope) = parent.child_call(host.principal.clone())?;
    let mut child = child.enter_facet(facet);
    let observation = crate::observability::start(
        &host.observations,
        crate::observability::Operation::PhysicalInvocation,
        "method",
        Some(&child),
        Some(object.id),
        None,
    );
    let observation_id = observation
        .as_ref()
        .map(crate::observability::Span::id)
        .or(child.observation_id());
    child = child.with_observation_id(observation_id);
    child.observe_cancellation();
    let entry = Rc::new(InvocationEntry {
        observation,
        host: Rc::downgrade(host),
        object: object.clone(),
        implementation: Some(implementation),
        receipt: receipt.map(|receipt| receipt.state.clone()),
        context: Some(child.clone()),
        installed,
        version: child.code_version(),
        id,
    });
    if let Some(receipt) = receipt {
        *receipt.state.entry.borrow_mut() = Rc::downgrade(&entry);
        receipt.state.entered.set(true);
    }
    child.set_invocation(Rc::downgrade(&entry));
    object.next_invocation.set(id);
    object.invocations.set(count);
    object.active_contexts.borrow_mut().insert(
        id,
        ActiveInvocation {
            version: child.code_version(),
            cancellation: child.cancellation().clone(),
        },
    );
    Ok((
        child,
        LogicalInvocationGuard {
            scope: Some(scope),
            entry: Some(entry),
        },
    ))
}

/// Pins an actual managed activity without minting receiver Context authority.
/// The caller owns this lease through physical callback settlement, including stop deadlines.
pub(crate) fn retain_activity(
    host: &Rc<HostInner>,
    object: &Rc<ObjectRecord>,
    installed: Rc<crate::host::reload::InstalledVersion>,
    cancellation: crate::CancellationToken,
) -> Result<InvocationLease> {
    host.epochs.exact(installed.version().get())?;
    if object.phase.get() != Phase::Live {
        return Err(RuntimeError::new(
            ErrorCode::Gone,
            "activity_owner",
            "activity owner is not live",
        ));
    }
    let limit = || RuntimeError::unavailable("invocation_limit", "invocation counter exhausted");
    let id = object
        .next_invocation
        .get()
        .checked_add(1)
        .ok_or_else(limit)?;
    let count = object.invocations.get().checked_add(1).ok_or_else(limit)?;
    let receipt = InvocationReceipt::new();
    let observation = crate::observability::start(
        &host.observations,
        crate::observability::Operation::PhysicalInvocation,
        "activity",
        None,
        Some(object.id),
        None,
    );
    let entry = Rc::new(InvocationEntry {
        observation,
        host: Rc::downgrade(host),
        object: object.clone(),
        implementation: Some(installed.implementation.clone()),
        receipt: Some(receipt.state.clone()),
        context: None,
        version: installed.version().get(),
        installed: Some(installed),
        id,
    });
    receipt.state.entered.set(true);
    *receipt.state.entry.borrow_mut() = Rc::downgrade(&entry);
    object.next_invocation.set(id);
    object.invocations.set(count);
    object.active_contexts.borrow_mut().insert(
        id,
        ActiveInvocation {
            version: entry.version,
            cancellation,
        },
    );
    Ok(InvocationLease { _entry: entry })
}

impl Drop for InvocationEntry {
    fn drop(&mut self) {
        drop(self.implementation.take());
        drop(self.installed.take());
        let token = self.object.active_contexts.borrow_mut().remove(&self.id);
        drop(token);
        self.object
            .invocations
            .set(self.object.invocations.get() - 1);
        if self.object.invocations.get() == 0 {
            let waiters = std::mem::take(&mut *self.object.idle_waiters.borrow_mut());
            for waiter in waiters {
                let _ = waiter.send(());
            }
        }
        if let Some(host) = self.host.upgrade() {
            host.roots.notify_change();
            host.idle(self.object.clone());
        }
        if let Some(mut observation) = self.observation.take() {
            observation.finish(crate::observability::Outcome::Settled);
        }
        if let Some(receipt) = self.receipt.take() {
            receipt.finish();
        }
    }
}
