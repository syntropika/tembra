use super::*;
use crate::CancellationToken;
use std::time::Duration;

pub(crate) struct IdleTicket {
    pub(crate) deadline: Duration,
    cancelled: CancellationToken,
}

impl IdleTicket {
    pub(crate) fn new(deadline: Duration) -> Rc<Self> {
        Rc::new(Self {
            deadline,
            cancelled: CancellationToken::new(),
        })
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.cancel(RuntimeError::aborted(
            "idle_cancelled",
            "idle interval ended",
        ));
    }
}

impl RemoteBinding {
    pub(crate) fn idle_ticket(&self) -> Option<Rc<IdleTicket>> {
        self.idle.borrow().clone()
    }

    pub(crate) fn applied_shape(&self) -> bool {
        self.current.borrow().is_some() && self.terminal.borrow().is_none()
    }

    pub(crate) fn begin_idle(&self, ticket: Rc<IdleTicket>) {
        *self.idle.borrow_mut() = Some(ticket.clone());
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let delay = ticket.deadline.saturating_sub(host.driver.now());
        let sleep = host.driver.sleep(delay);
        let weak = self.weak.clone();
        host.driver.spawn(
            async move {
                let cancelled = ticket.cancelled.cancelled();
                if matches!(
                    futures::future::select(sleep, cancelled).await,
                    futures::future::Either::Left(_)
                ) && let Some(binding) = weak.upgrade()
                {
                    binding.expire_idle(&ticket);
                }
            }
            .boxed_local(),
        );
    }

    pub(crate) fn expire_idle(&self, ticket: &Rc<IdleTicket>) {
        let exact = self
            .idle
            .borrow()
            .as_ref()
            .is_some_and(|old| Rc::ptr_eq(old, ticket));
        if exact && self.claims().is_empty() {
            self.release_binding();
        }
    }

    pub(super) fn cancel_idle(&self) {
        let ticket = self.idle.borrow_mut().take();
        if ticket.is_some()
            && let Some(host) = self.host.upgrade()
        {
            host.rpc_remove_idle(self.id);
        }
        if let Some(ticket) = ticket {
            ticket.cancel();
        }
    }

    pub(crate) fn reusable(&self) -> bool {
        if let Some(delegate) = self.delegate.borrow().clone() {
            return delegate.reusable();
        }
        if let Some(ticket) = self.idle_ticket() {
            let expired = self
                .host
                .upgrade()
                .is_none_or(|host| host.driver.now() >= ticket.deadline);
            if expired {
                self.expire_idle(&ticket);
            }
        }
        self.terminal.borrow().is_none()
    }

    pub(crate) fn idle_connection(&self) -> Option<ConnectionId> {
        self.idle.borrow().as_ref()?;
        self.attachment
            .borrow()
            .as_ref()
            .map(|attachment| attachment.connection)
    }

    pub(super) fn owned(&self) -> bool {
        !self.claims().is_empty()
            || self.idle_ticket().is_some_and(|ticket| {
                self.host
                    .upgrade()
                    .is_some_and(|host| host.driver.now() < ticket.deadline)
            })
    }
}
