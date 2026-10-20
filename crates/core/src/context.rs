use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context as TaskContext, Poll, Waker};

use futures::future::LocalBoxFuture;
use serde_json::Value;

use crate::error::{ErrorCode, Result, RuntimeError};

/// A local cancellation tree. Cancelling one child never cancels its siblings.
#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Rc<RefCell<CancellationState>>,
}

#[derive(Default)]
struct CancellationState {
    reason: Option<RuntimeError>,
    children: Vec<Weak<RefCell<CancellationState>>>,
    waiters: BTreeMap<u64, Waker>,
    next_waiter: u64,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn child(&self) -> Self {
        let child = Self::new();
        self.link_child(&child);
        child
    }

    /// Links cancellation to an existing child without changing its other parents.
    pub(crate) fn link_child(&self, child: &Self) {
        let reason = {
            let mut state = self.inner.borrow_mut();
            state.children.retain(|entry| entry.strong_count() > 0);
            state.children.push(Rc::downgrade(&child.inner));
            state.reason.clone()
        };
        if let Some(reason) = reason {
            child.cancel(reason);
        }
    }

    /// The first reason wins. Wakers run after all internal borrows are released.
    pub fn cancel(&self, reason: impl Into<RuntimeError>) {
        let reason = reason.into();
        let mut pending = vec![self.inner.clone()];
        let mut wake = Vec::new();
        while let Some(inner) = pending.pop() {
            let mut state = inner.borrow_mut();
            if state.reason.is_some() {
                continue;
            }
            state.reason = Some(reason.clone());
            pending.extend(state.children.drain(..).filter_map(|child| child.upgrade()));
            wake.extend(std::mem::take(&mut state.waiters).into_values());
        }
        for waiter in wake {
            waiter.wake();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.borrow().reason.is_some()
    }

    /// Checks cancellation without changing the token.
    ///
    /// # Errors
    /// Returns the original first cancellation reason, including its category and details.
    pub fn check(&self) -> Result<()> {
        match self.inner.borrow().reason.clone() {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// Resolves to the original reason, including when cancellation predates polling.
    pub fn cancelled(&self) -> LocalBoxFuture<'static, RuntimeError> {
        Box::pin(Cancelled {
            token: self.clone(),
            waiter: None,
        })
    }
}

struct Cancelled {
    token: CancellationToken,
    waiter: Option<u64>,
}

impl Future for Cancelled {
    type Output = RuntimeError;

    fn poll(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let next_waker = context.waker().clone();
        let mut state = this.token.inner.borrow_mut();
        if let Some(reason) = &state.reason {
            return Poll::Ready(reason.clone());
        }
        let waiter = *this.waiter.get_or_insert_with(|| {
            let waiter = state.next_waiter;
            state.next_waiter = state
                .next_waiter
                .checked_add(1)
                .expect("waiter ID overflow");
            waiter
        });
        let previous = state.waiters.insert(waiter, next_waker);
        drop(state);
        drop(previous);
        Poll::Pending
    }
}

impl Drop for Cancelled {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter {
            let previous = self.token.inner.borrow_mut().waiters.remove(&waiter);
            drop(previous);
        }
    }
}

#[derive(Default)]
struct ScopeState {
    settled: bool,
    expiry: CancellationToken,
    hooks: BTreeMap<u64, Box<dyn FnOnce()>>,
    next_hook: u64,
    children: Vec<Weak<RefCell<ScopeState>>>,
}

/// A host-created invocation capability; retaining a clone does not extend its scope.
#[derive(Clone)]
pub struct Context {
    host_id: u64,
    root_id: u64,
    code_version: u64,
    principal: Value,
    cancellation: CancellationToken,
    scope: Rc<RefCell<ScopeState>>,
    attachments: Rc<RefCell<HashMap<TypeId, Rc<dyn Any>>>>,
    invocation: Weak<crate::invocation::InvocationEntry>,
}

impl Context {
    pub(crate) fn new_root(
        host_id: u64,
        root_id: u64,
        code_version: u64,
        principal: Value,
    ) -> (Self, ScopeGuard) {
        let scope = Rc::new(RefCell::new(ScopeState::default()));
        let guard = ScopeGuard {
            scope: Some(scope.clone()),
        };
        (
            Self {
                host_id,
                root_id,
                code_version,
                principal,
                cancellation: CancellationToken::new(),
                scope,
                attachments: Rc::default(),
                invocation: Weak::new(),
            },
            guard,
        )
    }

    pub(crate) fn same_scope(&self, other: &Self) -> bool {
        self.host_id == other.host_id && Rc::ptr_eq(&self.scope, &other.scope)
    }

    pub(crate) fn set_invocation(&mut self, entry: Weak<crate::invocation::InvocationEntry>) {
        self.invocation = entry;
    }

    pub(crate) fn invocation(&self) -> Option<Rc<crate::invocation::InvocationEntry>> {
        self.invocation.upgrade()
    }

    pub(crate) fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn host_id(&self) -> u64 {
        self.host_id
    }

    pub fn root_id(&self) -> u64 {
        self.root_id
    }

    pub fn code_version(&self) -> u64 {
        self.code_version
    }

    pub fn principal(&self) -> &Value {
        &self.principal
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Settlement wakes pending scope-owned work without changing caller cancellation.
    pub(crate) fn scope_ended(&self) -> LocalBoxFuture<'static, RuntimeError> {
        self.scope.borrow().expiry.cancelled()
    }

    pub fn cancel(&self, reason: impl Into<RuntimeError>) {
        self.cancellation.cancel(reason);
    }

    /// Checks this capability against the intended host without mutating it.
    ///
    /// # Errors
    /// Returns `invalid/foreign_ctx` for another host, `aborted/expired` after scope
    /// settlement, or a cancellation error (preserving `superseded` when applicable).
    pub fn check(&self, host_id: u64) -> Result<()> {
        if host_id != self.host_id {
            return Err(RuntimeError::invalid(
                "foreign_ctx",
                "context belongs to another host",
            ));
        }
        if self.scope.borrow().settled {
            return Err(RuntimeError::aborted(
                "expired",
                "context scope has already settled",
            ));
        }
        if let Some(attachment) = self.attachment::<crate::host::HostAttachment>()
            && let Some(host) = attachment.0.upgrade()
        {
            host.epochs.exact(self.code_version)?;
        }
        if let Err(reason) = self.cancellation.check() {
            if reason.code == ErrorCode::Superseded {
                return Err(reason);
            }
            let (_, reason, message, details) = reason.into_parts();
            return Err(RuntimeError::aborted(reason, message).with_details(details));
        }
        Ok(())
    }

    /// Creates an independently owned child scope sharing the root identity.
    ///
    /// Parent settlement expires the child; dropping its returned guard expires only that
    /// subtree. Cancellation flows from parent to child, never from child to siblings.
    ///
    /// # Errors
    /// Returns [`Self::check`] failures without creating a child.
    pub fn child_call(&self, principal: Value) -> Result<(Self, ScopeGuard)> {
        self.check(self.host_id)?;
        let scope = Rc::new(RefCell::new(ScopeState::default()));
        {
            let mut parent = self.scope.borrow_mut();
            parent.children.retain(|child| child.strong_count() > 0);
            parent.children.push(Rc::downgrade(&scope));
        }
        let guard = ScopeGuard {
            scope: Some(scope.clone()),
        };
        Ok((
            Self {
                host_id: self.host_id,
                root_id: self.root_id,
                code_version: self.code_version,
                principal,
                cancellation: self.cancellation.child(),
                scope,
                attachments: Rc::new(RefCell::new(self.attachments.borrow().clone())),
                invocation: self.invocation.clone(),
            },
            guard,
        ))
    }

    /// Registers a cleanup hook, run once when this scope settles.
    ///
    /// Hooks run without a context-state borrow. Cancellation alone does not settle a scope
    /// and does not prevent cleanup registration. Hooks should not panic.
    ///
    /// # Errors
    /// Returns `aborted/expired` if already settled, without registering the hook.
    pub fn on_settle(&self, hook: impl FnOnce() + 'static) -> Result<()> {
        let subscription = self.subscribe_settle(hook)?;
        subscription.id.set(None);
        Ok(())
    }

    /// Observes scope settlement until explicitly removed or this subscription is dropped.
    ///
    /// Removal drops callback captures after releasing the scope borrow. The weak
    /// subscription does not keep its Context or owning call alive. Cancellation alone
    /// does not settle a scope. A hook already selected by settlement cannot be withdrawn.
    ///
    /// # Errors
    /// Refuses an expired scope or exhausted registration identity before inserting the hook.
    ///
    /// ```
    /// use std::{cell::Cell, rc::Rc};
    /// use futures::FutureExt;
    /// use tembra_core::{Host, HostOptions, ManualDriver};
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("scope", serde_json::Value::Null), driver.clone());
    /// let called = Rc::new(Cell::new(false));
    /// let observed = called.clone();
    /// driver.run_until(host.root(move |context| async move {
    ///     let subscription = context.subscribe_settle(move || observed.set(true))?;
    ///     drop(subscription); // The callback is removed before root settlement.
    ///     Ok(())
    /// }.boxed_local()))?;
    /// assert!(!called.get());
    /// driver.run_until(host.close())?;
    /// # Ok::<(), tembra_core::RuntimeError>(())
    /// ```
    #[must_use = "retain the subscription until settlement, or use on_settle for a permanent hook"]
    pub fn subscribe_settle(&self, hook: impl FnOnce() + 'static) -> Result<SettleSubscription> {
        let mut state = self.scope.borrow_mut();
        if state.settled {
            return Err(RuntimeError::aborted(
                "expired",
                "context scope has already settled",
            ));
        }
        let id = state.next_hook.checked_add(1).ok_or_else(|| {
            RuntimeError::unavailable("scope_hooks", "scope hook identity exhausted")
        })?;
        state.next_hook = id;
        state.hooks.insert(id, Box::new(hook));
        Ok(SettleSubscription {
            scope: Rc::downgrade(&self.scope),
            id: std::cell::Cell::new(Some(id)),
        })
    }

    pub(crate) fn set_attachment<T: Any>(&self, value: Rc<T>) {
        self.attachments
            .borrow_mut()
            .insert(TypeId::of::<T>(), value);
    }

    pub(crate) fn attachment<T: Any>(&self) -> Option<Rc<T>> {
        self.attachments
            .borrow()
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|value| value.downcast().ok())
    }
}

/// A removable, weak observation of one Context scope's settlement.
/// Dropping or unsubscribing releases callback captures without invoking the callback.
#[must_use = "dropping the subscription removes its unsettled callback"]
pub struct SettleSubscription {
    scope: Weak<RefCell<ScopeState>>,
    id: std::cell::Cell<Option<u64>>,
}
impl SettleSubscription {
    /// Removes a callback that has not already been selected by settlement.
    pub fn unsubscribe(&self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let callback = self
            .scope
            .upgrade()
            .and_then(|scope| scope.borrow_mut().hooks.remove(&id));
        drop(callback);
    }
}
impl Drop for SettleSubscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// The owning call settles its scope explicitly or by dropping this guard.
pub struct ScopeGuard {
    scope: Option<Rc<RefCell<ScopeState>>>,
}

impl ScopeGuard {
    pub fn settle(&mut self) {
        let Some(scope) = self.scope.take() else {
            return;
        };
        let mut pending = vec![scope];
        let mut settled = Vec::new();
        while let Some(scope) = pending.pop() {
            let mut state = scope.borrow_mut();
            if state.settled {
                continue;
            }
            state.settled = true;
            pending.extend(state.children.drain(..).filter_map(|child| child.upgrade()));
            settled.push((state.expiry.clone(), std::mem::take(&mut state.hooks)));
        }
        // All descendants are expired before wakers or hooks can reenter any context.
        for (expiry, _) in &settled {
            expiry.cancel(RuntimeError::aborted(
                "expired",
                "context scope has already settled",
            ));
        }
        for (_, group) in settled.into_iter().rev() {
            for hook in group.into_values() {
                hook();
            }
        }
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        self.settle();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use futures::{FutureExt, executor::block_on};
    use serde_json::json;

    use super::*;

    #[test]
    fn child_scope_preserves_root_and_has_independent_claims() {
        let (root, root_guard) = Context::new_root(7, 41, 2, json!("guest"));
        let (child, child_guard) = root.child_call(json!("host")).unwrap();
        assert_eq!(
            (child.host_id(), child.root_id(), child.code_version()),
            (7, 41, 2)
        );
        assert_eq!(root.principal(), &json!("guest"));
        assert_eq!(child.principal(), &json!("host"));
        let claims = Rc::new(Cell::new(2));
        let released = claims.clone();
        root.on_settle(move || released.set(released.get() - 1))
            .unwrap();
        let released = claims.clone();
        child
            .on_settle(move || released.set(released.get() - 1))
            .unwrap();
        drop(child_guard);
        assert_eq!(claims.get(), 1);
        assert_eq!(child.check(7).unwrap_err().reason, "expired");
        assert!(root.check(7).is_ok());
        drop(root_guard);
        assert_eq!(claims.get(), 0);
    }

    #[test]
    fn root_settlement_expires_descendants_before_reentrant_hooks() {
        let (root, mut guard) = Context::new_root(1, 1, 1, Value::Null);
        let (child, _child_guard) = root.child_call(Value::Null).unwrap();
        let checked = Rc::new(Cell::new(false));
        let seen = checked.clone();
        root.on_settle(move || {
            assert_eq!(child.check(1).unwrap_err().reason, "expired");
            seen.set(true);
        })
        .unwrap();
        guard.settle();
        guard.settle();
        assert!(checked.get());
        assert_eq!(
            root.child_call(Value::Null).err().unwrap().reason,
            "expired"
        );
        assert_eq!(root.on_settle(|| {}).unwrap_err().reason, "expired");
    }

    #[test]
    fn foreign_cancelled_and_superseded_contexts_are_refused() {
        let (root, _guard) = Context::new_root(1, 1, 1, Value::Null);
        assert_eq!(root.check(2).unwrap_err().reason, "foreign_ctx");
        let (child, _child_guard) = root.child_call(Value::Null).unwrap();
        child.cancel("child-stop");
        assert_eq!(child.check(1).unwrap_err().code, ErrorCode::Aborted);
        assert!(root.check(1).is_ok());
        root.cancel(RuntimeError::new(
            ErrorCode::Superseded,
            "retired",
            "old code retired",
        ));
        assert_eq!(root.check(1).unwrap_err().code, ErrorCode::Superseded);
        assert_eq!(child.check(1).unwrap_err().reason, "child-stop");
    }

    #[test]
    fn cancellation_inherits_and_waiters_can_be_dropped() {
        let parent = CancellationToken::new();
        let child = parent.child();
        let sibling = parent.child();
        let mut dropped = child.cancelled();
        assert!(dropped.as_mut().now_or_never().is_none());
        drop(dropped);
        assert!(child.inner.borrow().waiters.is_empty());
        parent.cancel("parent-stop");
        assert_eq!(block_on(child.cancelled()).reason, "parent-stop");
        assert_eq!(sibling.check().unwrap_err().reason, "parent-stop");
        assert_eq!(parent.child().check().unwrap_err().reason, "parent-stop");
    }

    #[test]
    fn child_inherits_typed_host_attachment_without_replacing_parent() {
        let (root, _guard) = Context::new_root(1, 1, 1, Value::Null);
        root.set_attachment(Rc::new(42_u64));
        let (child, _child_guard) = root.child_call(Value::Null).unwrap();
        assert_eq!(*child.attachment::<u64>().unwrap(), 42);
        child.set_attachment(Rc::new(9_u64));
        assert_eq!(*root.attachment::<u64>().unwrap(), 42);
        assert_eq!(*child.attachment::<u64>().unwrap(), 9);
    }

    #[test]
    fn parent_settlement_wakes_pending_descendant_scope_waiter() {
        use crate::driver::{Driver, ManualDriver};
        use futures::channel::oneshot;

        let driver = ManualDriver::new();
        let (root, guard) = Context::new_root(1, 1, 1, Value::Null);
        let (child, _child_guard) = root.child_call(Value::Null).unwrap();
        let expiry = child.scope_ended();
        let (send, receive) = oneshot::channel();
        driver.spawn(
            async move {
                send.send(expiry.await).unwrap();
            }
            .boxed_local(),
        );
        driver.run_until_stalled();
        drop(guard);
        let error = driver.run_until(receive).unwrap();
        assert_eq!(error.code, ErrorCode::Aborted);
        assert_eq!(error.reason, "expired");
        assert!(!root.cancellation().is_cancelled());
        assert!(!child.cancellation().is_cancelled());
        assert_eq!(child.check(1).unwrap_err().reason, "expired");
    }

    #[test]
    fn cancellation_does_not_prevent_registering_required_cleanup() {
        let (context, guard) = Context::new_root(1, 1, 1, Value::Null);
        let released = Rc::new(Cell::new(false));
        context.cancel("requested");
        let seen = released.clone();
        context.on_settle(move || seen.set(true)).unwrap();
        assert!(!released.get());
        drop(guard);
        assert!(released.get());
    }
}

#[cfg(test)]
mod cancellation_reentrancy {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{RawWaker, RawWakerVTable},
    };
    thread_local! {
        static PROBE: RefCell<Option<CancellationToken>> = const { RefCell::new(None) };
    }
    fn observe(data: *const ()) {
        PROBE.with(|probe| {
            if let Some(token) = probe.borrow().as_ref() {
                let _ = token.is_cancelled();
            }
        });
        // Each vtable operation owns or borrows a live Arc allocation.
        unsafe { &*data.cast::<AtomicUsize>() }.fetch_add(1, Ordering::Relaxed);
    }
    unsafe fn clone(data: *const ()) -> RawWaker {
        observe(data);
        unsafe { Arc::increment_strong_count(data.cast::<AtomicUsize>()) };
        RawWaker::new(data, &VTABLE)
    }
    unsafe fn wake(data: *const ()) {
        observe(data);
        unsafe { drop(Arc::from_raw(data.cast::<AtomicUsize>())) };
    }
    unsafe fn wake_by_ref(data: *const ()) {
        observe(data);
    }
    unsafe fn drop_waker(data: *const ()) {
        observe(data);
        unsafe { drop(Arc::from_raw(data.cast::<AtomicUsize>())) };
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

    #[test]
    fn cancellation_waker_clone_replace_drop_and_wake_allow_reentry() {
        let token = CancellationToken::new();
        PROBE.with(|probe| *probe.borrow_mut() = Some(token.clone()));
        let calls = Arc::new(AtomicUsize::new(0));
        let raw = RawWaker::new(Arc::into_raw(calls.clone()).cast(), &VTABLE);
        // Arc<AtomicUsize> is Send + Sync; callbacks hold no thread-affine raw data.
        let waker = unsafe { Waker::from_raw(raw) };
        let mut task = TaskContext::from_waker(&waker);
        let mut wait = token.cancelled();
        assert!(wait.as_mut().poll(&mut task).is_pending());
        assert!(wait.as_mut().poll(&mut task).is_pending());
        drop(wait);
        let mut wait = token.cancelled();
        assert!(wait.as_mut().poll(&mut task).is_pending());
        token.cancel(RuntimeError::aborted("probe", "cancelled"));
        assert!(
            matches!(wait.as_mut().poll(&mut task), Poll::Ready(error) if error.reason == "probe")
        );
        drop(wait);
        drop(waker);
        assert!(calls.load(Ordering::Relaxed) >= 6);
        PROBE.with(|probe| probe.borrow_mut().take());
    }
}

#[cfg(test)]
mod removable_settle_tests {
    use super::*;
    use std::cell::Cell;

    struct Reenter {
        context: Context,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Reenter {
        fn drop(&mut self) {
            self.context.check(self.context.host_id()).unwrap();
            self.context.on_settle(|| {}).unwrap();
            self.drops.set(self.drops.get() + 1);
        }
    }
    #[test]
    fn unsubscribe_releases_callback_captures_outside_the_scope_borrow() {
        let (context, guard) = Context::new_root(1, 1, 1, Value::Null);
        let drops = Rc::new(Cell::new(0));
        let reenter = Reenter {
            context: context.clone(),
            drops: drops.clone(),
        };
        let subscription = context.subscribe_settle(move || drop(reenter)).unwrap();
        subscription.unsubscribe();
        subscription.unsubscribe();
        assert_eq!(drops.get(), 1);
        drop(guard);
    }
    #[test]
    fn repeated_removable_observers_do_not_accumulate_on_a_long_lived_root() {
        let (context, guard) = Context::new_root(1, 1, 1, Value::Null);
        let called = Rc::new(Cell::new(0));
        for _ in 0..10_000 {
            let selected = called.clone();
            let subscription = context
                .subscribe_settle(move || selected.set(selected.get() + 1))
                .unwrap();
            drop(subscription);
        }
        assert!(context.scope.borrow().hooks.is_empty());
        assert_eq!(Rc::strong_count(&called), 1);
        let selected = called.clone();
        context
            .on_settle(move || selected.set(selected.get() + 1))
            .unwrap();
        drop(guard);
        assert_eq!(
            called.get(),
            1,
            "permanent on_settle preserves its original behavior"
        );
    }
}
