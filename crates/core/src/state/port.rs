use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context as TaskContext, Poll, Waker},
};

use futures::future::{FutureExt, LocalBoxFuture};

use crate::{Result, RuntimeError};

use super::{
    ReadGuard, Reporter, Snapshot, StateEvent, gone,
    graph::{NodeId, StateGraph},
    pending,
    publication::{PendingPublication, PublicationQueue},
};

type Observer = Rc<dyn Fn(StateEvent) -> Result<()>>;
type InterestHook = Rc<dyn Fn(bool) -> Result<()>>;

struct Listener {
    callback: Observer,
    observation: Option<crate::observability::Span>,
    seen: bool,
}

#[derive(Default)]
struct PortState {
    current: Option<Snapshot>,
    ended: Option<RuntimeError>,
    listeners: BTreeMap<u64, Listener>,
    waiters: BTreeMap<u64, Waker>,
}

pub(crate) struct ReadPort {
    state: RefCell<PortState>,
    next: Cell<u64>,
    read_guard: ReadGuard,
    subscribe_guard: ReadGuard,
    reporter: Reporter,
    _queue: Rc<PublicationQueue>,
    interest: RefCell<Option<InterestHook>>,
    interested: Cell<bool>,
}

impl ReadPort {
    pub fn new(
        read_guard: ReadGuard,
        subscribe_guard: ReadGuard,
        reporter: Reporter,
        queue: Rc<PublicationQueue>,
    ) -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::default(),
            next: Cell::new(0),
            read_guard,
            subscribe_guard,
            reporter,
            _queue: queue,
            interest: RefCell::new(None),
            interested: Cell::new(false),
        })
    }

    pub(crate) fn set_interest_hook(&self, hook: InterestHook) {
        *self.interest.borrow_mut() = Some(hook);
    }

    fn update_interest(&self) -> Result<()> {
        let wanted = {
            let state = self.state.borrow();
            state.ended.is_none() && (!state.listeners.is_empty() || !state.waiters.is_empty())
        };
        if self.interested.replace(wanted) != wanted {
            let hook = self.interest.borrow().clone();
            if let Some(hook) = hook {
                hook(wanted)?;
            }
        }
        Ok(())
    }

    pub fn readable(self: &Rc<Self>) -> Readable {
        Readable {
            port: Rc::downgrade(self),
            origin: None,
        }
    }

    pub(super) fn node_readable(self: &Rc<Self>, graph: &Rc<StateGraph>, node: NodeId) -> Readable {
        Readable {
            port: Rc::downgrade(self),
            origin: Some((Rc::downgrade(graph), node)),
        }
    }

    pub fn current(&self) -> Result<Snapshot> {
        let state = self.state.borrow();
        if let Some(error) = &state.ended {
            return Err(error.clone());
        }
        state.current.clone().ok_or_else(pending)
    }

    pub fn is_terminated(&self) -> bool {
        self.state.borrow().ended.is_some()
    }

    pub fn seed(&self, snapshot: Snapshot) {
        let mut state = self.state.borrow_mut();
        debug_assert!(state.current.is_none() && state.waiters.is_empty());
        if state.ended.is_none() {
            state.current = Some(snapshot);
        }
    }

    pub(super) fn initialize_silent(&self, snapshot: Snapshot) -> Vec<Waker> {
        let mut state = self.state.borrow_mut();
        state.current = Some(snapshot);
        std::mem::take(&mut state.waiters).into_values().collect()
    }

    pub fn adopt(self: &Rc<Self>, event: StateEvent) -> Option<PendingPublication> {
        let mut state = self.state.borrow_mut();
        if state.ended.is_some() {
            return None;
        }
        state.current = Some(event.snapshot().clone());
        let initial = matches!(event, StateEvent::Snapshot(_));
        Some(PendingPublication {
            port: Rc::downgrade(self),
            event,
            listeners: state
                .listeners
                .iter()
                .filter_map(|(id, listener)| (!initial || !listener.seen).then_some(*id))
                .collect(),
        })
    }

    pub fn terminate(&self, error: RuntimeError) {
        let (listeners, waiters) = {
            let mut state = self.state.borrow_mut();
            if state.ended.is_some() {
                return;
            }
            state.ended = Some(error);
            (
                std::mem::take(&mut state.listeners),
                std::mem::take(&mut state.waiters),
            )
        };
        drop(listeners);
        if let Err(error) = self.update_interest() {
            (self.reporter)(error);
        }
        for waiter in waiters.into_values() {
            waiter.wake();
        }
    }

    pub(super) fn deliver(&self, event: StateEvent, listeners: Vec<u64>) {
        let waiters = std::mem::take(&mut self.state.borrow_mut().waiters);
        if let Err(error) = self.update_interest() {
            (self.reporter)(error);
        }
        for waiter in waiters.into_values() {
            waiter.wake();
        }
        for id in listeners {
            let target = {
                let mut state = self.state.borrow_mut();
                if state.ended.is_some() {
                    return;
                }
                state.listeners.get_mut(&id).map(|listener| {
                    let first = !std::mem::replace(&mut listener.seen, true);
                    (listener.callback.clone(), first)
                })
            };
            if let Some((callback, first)) = target {
                let event = if first {
                    StateEvent::Snapshot(event.snapshot().clone())
                } else {
                    event.clone()
                };
                let mut observation = crate::observability::start(
                    self._queue.observations(),
                    crate::observability::Operation::StateDelivery,
                    "observer",
                    None,
                    None,
                    None,
                );
                let result = callback(event);
                if let Some(observation) = &mut observation {
                    observation.result(&result);
                }
                if let Err(error) = result {
                    (self.reporter)(error);
                }
            }
        }
    }

    fn subscribe(self: &Rc<Self>, callback: Observer) -> Result<Subscription> {
        (self.subscribe_guard)()?;
        self.subscribe_staged(callback, true)
    }

    pub(in crate::state) fn subscribe_staged(
        self: &Rc<Self>,
        callback: Observer,
        deliver_initial: bool,
    ) -> Result<Subscription> {
        let id = {
            let mut state = self.state.borrow_mut();
            if let Some(error) = &state.ended {
                return Err(error.clone());
            }
            let id = self.next.get() + 1;
            self.next.set(id);
            state.listeners.insert(
                id,
                Listener {
                    callback: callback.clone(),
                    observation: crate::observability::start(
                        self._queue.observations(),
                        crate::observability::Operation::StateSubscription,
                        "subscription",
                        None,
                        None,
                        None,
                    ),
                    seen: false,
                },
            );
            id
        };
        let subscription = Subscription {
            port: Rc::downgrade(self),
            id: Cell::new(Some(id)),
        };
        self.update_interest()?;
        let current = {
            let mut state = self.state.borrow_mut();
            let current = deliver_initial.then(|| state.current.clone()).flatten();
            state.listeners.get_mut(&id).and_then(|listener| {
                if listener.seen {
                    return None;
                }
                if current.is_some() {
                    listener.seen = true;
                }
                current
            })
        };
        if let Some(snapshot) = current
            && let Err(error) = callback(StateEvent::Snapshot(snapshot))
        {
            (self.reporter)(error);
        }
        Ok(subscription)
    }

    fn unregister(&self, id: u64) {
        let callback = self.state.borrow_mut().listeners.remove(&id);
        if let Some(mut callback) = callback {
            if let Some(mut observation) = callback.observation.take() {
                observation.finish(crate::observability::Outcome::Ok);
            }
            drop(callback);
        }
        if let Err(error) = self.update_interest() {
            (self.reporter)(error);
        }
    }
}

impl Drop for ReadPort {
    fn drop(&mut self) {
        let waiters = std::mem::take(&mut self.state.get_mut().waiters);
        for waiter in waiters.into_values() {
            waiter.wake();
        }
    }
}

/// A cloneable weak capability to read state without retaining its managed authority.
#[derive(Clone)]
pub struct Readable {
    pub(super) port: Weak<ReadPort>,
    pub(super) origin: Option<(Weak<StateGraph>, NodeId)>,
}

impl Readable {
    /// Returns the current immutable snapshot.
    ///
    /// # Errors
    /// Refuses ended authority, pending initialization and disallowed graph reentrancy.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let port = self.port.upgrade().ok_or_else(gone)?;
        (port.read_guard)()?;
        port.current()
    }

    /// Reads the private candidate view authorized by an actual initializer Context.
    /// Outside that private authority this uses the ordinary installed snapshot.
    /// # Errors
    /// Refuses expired/cancelled contexts, pending candidate values, cycles and ended state.
    /// The context never grants an ordinary caller access to uncommitted values.
    pub fn snapshot_in(&self, context: &crate::Context) -> Result<Snapshot> {
        super::CandidateSources::snapshot_in(self, context)
    }

    /// Waits weakly for first initialization; dropping the future unregisters its waiter.
    ///
    /// # Errors
    /// Returns termination or admission errors. This does not extend a reference claim,
    /// object lifetime or caller scope, and does not cancel independently owned work.
    pub fn await_hydrated(&self) -> LocalBoxFuture<'static, Result<Snapshot>> {
        HydrationWait {
            port: self.port.clone(),
            id: None,
        }
        .boxed_local()
    }

    /// Delivers the current snapshot immediately when available, then committed snapshots.
    ///
    /// # Errors
    /// Refuses terminated authority or subscription admission, including factory access.
    /// Callback `Err` values are reported separately; native panics propagate after any
    /// already committed effect and may interrupt later delivery.
    pub fn subscribe(
        &self,
        callback: impl Fn(Snapshot) -> Result<()> + 'static,
    ) -> Result<Subscription> {
        self.subscribe_events(move |event| callback(event.snapshot().clone()))
    }

    /// Subscribes to first snapshot, operations and reset events in publication order.
    ///
    /// # Errors
    /// Has the admission and callback behavior of [`Self::subscribe`]. A pending source
    /// delivers its first snapshot when hydrated rather than inventing a null value.
    pub fn subscribe_events(
        &self,
        callback: impl Fn(StateEvent) -> Result<()> + 'static,
    ) -> Result<Subscription> {
        self.port
            .upgrade()
            .ok_or_else(gone)?
            .subscribe(Rc::new(callback))
    }

    pub(crate) fn same_source(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.port, &other.port)
    }
}

/// An idempotently removable weak subscription. Dropping it unsubscribes.
pub struct Subscription {
    port: Weak<ReadPort>,
    id: Cell<Option<u64>>,
}

impl Subscription {
    pub fn unsubscribe(&self) {
        if let Some(id) = self.id.take()
            && let Some(port) = self.port.upgrade()
        {
            port.unregister(id);
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

struct HydrationWait {
    port: Weak<ReadPort>,
    id: Option<u64>,
}

impl Future for HydrationWait {
    type Output = Result<Snapshot>;

    fn poll(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let Some(port) = self.port.upgrade() else {
            return Poll::Ready(Err(gone()));
        };
        if let Err(error) = (port.read_guard)() {
            return Poll::Ready(Err(error));
        }
        if port.interest.borrow().is_some()
            && let Err(error) = (port.subscribe_guard)()
        {
            return Poll::Ready(Err(error));
        }
        let next_waker = context.waker().clone();
        let mut state = port.state.borrow_mut();
        if let Some(error) = &state.ended {
            return Poll::Ready(Err(error.clone()));
        }
        if let Some(snapshot) = &state.current {
            return Poll::Ready(Ok(snapshot.clone()));
        }
        let id = *self.id.get_or_insert_with(|| {
            let id = port.next.get() + 1;
            port.next.set(id);
            id
        });
        let previous = state.waiters.insert(id, next_waker);
        drop(state);
        drop(previous);
        if let Err(error) = port.update_interest() {
            if let Some(id) = self.id.take() {
                let previous = port.state.borrow_mut().waiters.remove(&id);
                drop(previous);
            }
            let _ = port.update_interest();
            return Poll::Ready(Err(error));
        }
        Poll::Pending
    }
}

impl Drop for HydrationWait {
    fn drop(&mut self) {
        if let Some(port) = self.port.upgrade()
            && let Some(id) = self.id
        {
            let previous = port.state.borrow_mut().waiters.remove(&id);
            drop(previous);
            if let Err(error) = port.update_interest() {
                (port.reporter)(error);
            }
        }
    }
}
