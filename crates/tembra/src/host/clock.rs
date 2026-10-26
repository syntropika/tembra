//! Only the native driver owns this timer. Shared JS runtimes use the same clock origin.
use super::*;
use futures::future::{AbortHandle, Abortable};

impl Host {
    /// Read the monotonic host clock used by both native and embedded adapters.
    pub fn now_ms(&self) -> u64 {
        self.0.clock.now_ms()
    }

    pub(super) fn tick(&self) {
        if let Err(error) = self.kernel().tick(self.now_ms()) {
            self.0.errors.borrow_mut().push(error);
        }
    }

    pub(super) fn schedule_deadline(&self) {
        if self.0.external_actions.get() {
            return;
        }
        let deadline = self.kernel().next_deadline();
        if self.0.deadline.borrow().as_ref().map(|(at, _)| *at) == deadline {
            return;
        }
        if let Some((_, timer)) = self.0.deadline.borrow_mut().take() {
            timer.abort();
        }
        let Some(deadline) = deadline else {
            return;
        };
        let (abort, registration) = AbortHandle::new_pair();
        let wait = self.0.clock.sleep(deadline.saturating_sub(self.now_ms()));
        let weak = Rc::downgrade(&self.0);
        *self.0.deadline.borrow_mut() = Some((deadline, abort));
        if let Err(error) = self.0.executor.spawn_local(async move {
            if Abortable::new(wait, registration).await.is_err() {
                return;
            }
            if let Some(inner) = weak.upgrade() {
                let host = Host(inner);
                host.0.deadline.borrow_mut().take();
                host.flush();
            }
        }) {
            self.0.deadline.borrow_mut().take();
            self.0
                .errors
                .borrow_mut()
                .push(Error::invalid(Reason::Executor, error.to_string()));
        }
    }
}
