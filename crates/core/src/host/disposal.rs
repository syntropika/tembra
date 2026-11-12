//! Exact-instance physical disposal notifications; waiting never reserves a new instance.
use super::*;
use std::{
    future::Future,
    pin::Pin,
    rc::Weak,
    task::{Context, Poll, Waker},
};

type Waiter = Rc<RefCell<Option<Waker>>>;
pub(super) type DisposalWaiters = BTreeMap<Id, Vec<Weak<RefCell<Option<Waker>>>>>;

/// Wait for one retained instance's physical disposal or host shutdown.
/// Dropping the future unregisters the waiter without affecting cleanup.
pub struct Disposal {
    host: Host,
    object: Id,
    waiter: Waiter,
}

impl Host {
    /// Observe only the closing instance currently reserved at this exact address.
    /// Returns `None` if the address is free, live, or the host is shutting down.
    /// Completion does not grant admission: callers must acquire again with every check repeated.
    pub fn wait_for_disposal(&self, address: &Address) -> Option<Disposal> {
        let mut inner = self.inner.borrow_mut();
        if inner.closing {
            return None;
        }
        let object = *inner.addresses.get(address)?;
        let record = inner.objects.get(&object)?;
        if !record.disposing && record.phase != Status::Gone {
            return None;
        }
        let waiter = Rc::new(RefCell::new(None));
        inner
            .disposal_waiters
            .entry(object)
            .or_default()
            .push(Rc::downgrade(&waiter));
        Some(Disposal {
            host: self.clone(),
            object,
            waiter,
        })
    }
}

impl Future for Disposal {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let inner = self.host.inner.borrow();
        if inner.closing || !inner.objects.contains_key(&self.object) {
            Poll::Ready(())
        } else {
            self.waiter.borrow_mut().replace(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Drop for Disposal {
    fn drop(&mut self) {
        let mut inner = self.host.inner.borrow_mut();
        if let Some(waiters) = inner.disposal_waiters.get_mut(&self.object) {
            let mine = Rc::downgrade(&self.waiter);
            waiters.retain(|waiter| !waiter.ptr_eq(&mine));
            if waiters.is_empty() {
                inner.disposal_waiters.remove(&self.object);
            }
        }
    }
}

/// Wake outside the kernel borrow: an embedding may synchronously poll its executor.
pub(super) fn wake(waiters: impl IntoIterator<Item = Weak<RefCell<Option<Waker>>>>) {
    for waiter in waiters {
        if let Some(waiter) = waiter.upgrade() {
            let waker = waiter.borrow_mut().take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}
