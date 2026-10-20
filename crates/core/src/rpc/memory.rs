use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context, Poll, Waker},
};

use futures::{Stream, stream::StreamExt};

use super::{CloseReason, Link, LinkEvent, LinkTx, SendError};
use crate::{Driver, Result, RuntimeError};

/// Bounded message storage in each MemoryLink direction, including received events.
#[derive(Clone, Debug)]
pub struct MemoryLinkOptions {
    pub max_message_bytes: usize,
    pub queued_bytes: usize,
    pub queued_messages: usize,
}
impl Default for MemoryLinkOptions {
    fn default() -> Self {
        Self {
            max_message_bytes: 1_048_576,
            queued_bytes: 4_194_304,
            queued_messages: 256,
        }
    }
}
struct Endpoint {
    driver: Rc<dyn Driver>,
    peer: RefCell<Weak<Endpoint>>,
    self_weak: Weak<Endpoint>,
    messages: RefCell<VecDeque<Rc<[u8]>>>,
    bytes: Cell<usize>,
    events: RefCell<VecDeque<LinkEvent>>,
    received_bytes: Cell<usize>,
    received_count: Cell<usize>,
    waiter: RefCell<Option<Waker>>,
    writable: Cell<bool>,
    paused: Cell<bool>,
    severed: Cell<bool>,
    closed: Cell<bool>,
    scheduled: Cell<bool>,
    options: MemoryLinkOptions,
}
impl Endpoint {
    fn wake(&self) {
        let waiter = self.waiter.borrow_mut().take();
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }
    fn event(&self, event: LinkEvent) {
        self.events.borrow_mut().push_back(event);
        self.wake();
    }
    fn schedule(&self) {
        if self.paused.get()
            || self.severed.get()
            || self.closed.get()
            || self.messages.borrow().is_empty()
            || self.scheduled.replace(true)
        {
            return;
        }
        let weak = self.self_weak.clone();
        self.driver.spawn(Box::pin(async move {
            let Some(endpoint) = weak.upgrade() else {
                return;
            };
            endpoint.scheduled.set(false);
            if endpoint.paused.get() || endpoint.severed.get() || endpoint.closed.get() {
                return;
            }
            let Some(peer) = endpoint.peer.borrow().upgrade() else {
                return;
            };
            if peer.closed.get() {
                return;
            }
            let message = endpoint.messages.borrow().front().cloned();
            if let Some(message) = message {
                if peer.received_count.get() >= peer.options.queued_messages
                    || message.len()
                        > peer
                            .options
                            .queued_bytes
                            .saturating_sub(peer.received_bytes.get())
                {
                    return;
                }
                endpoint.messages.borrow_mut().pop_front();
                endpoint.bytes.set(endpoint.bytes.get() - message.len());
                peer.received_bytes
                    .set(peer.received_bytes.get() + message.len());
                peer.received_count.set(peer.received_count.get() + 1);
                peer.event(LinkEvent::Message(message.to_vec()));
                if !endpoint.writable.replace(true) {
                    endpoint.event(LinkEvent::Writable);
                }
                endpoint.schedule();
            }
        }));
    }
    fn finish(&self, reason: CloseReason) {
        if self.closed.replace(true) {
            return;
        }
        self.messages.borrow_mut().clear();
        self.bytes.set(0);
        self.event(LinkEvent::Closed(reason));
    }
}
impl LinkTx for Endpoint {
    fn try_send(&self, utf8: Rc<[u8]>) -> std::result::Result<(), SendError> {
        if self.closed.get() {
            return Err(SendError::Closed);
        }
        if utf8.len() > self.options.max_message_bytes {
            return Err(SendError::TooLarge);
        }
        if self.messages.borrow().len() >= self.options.queued_messages
            || utf8.len() > self.options.queued_bytes.saturating_sub(self.bytes.get())
        {
            return Err(SendError::WouldBlock);
        }
        self.bytes.set(self.bytes.get() + utf8.len());
        self.messages.borrow_mut().push_back(utf8);
        self.schedule();
        Ok(())
    }
    fn close(&self, reason: CloseReason) {
        self.finish(reason.clone());
        let peer = self.peer.borrow().upgrade();
        if let Some(peer) = peer {
            peer.finish(reason);
        }
    }
}
struct Events(Rc<Endpoint>);
impl Stream for Events {
    type Item = LinkEvent;
    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<LinkEvent>> {
        let event = self.0.events.borrow_mut().pop_front();
        if let Some(event) = event {
            match &event {
                LinkEvent::Message(bytes) => {
                    self.0
                        .received_bytes
                        .set(self.0.received_bytes.get() - bytes.len());
                    self.0.received_count.set(self.0.received_count.get() - 1);
                    let peer = self.0.peer.borrow().upgrade();
                    if let Some(peer) = peer {
                        peer.schedule();
                    }
                }
                LinkEvent::Writable => self.0.writable.set(false),
                _ => {}
            }
            return Poll::Ready(Some(event));
        }
        if self.0.closed.get() {
            return Poll::Ready(None);
        }
        let candidate = context.waker().clone();
        let previous = self.0.waiter.borrow_mut().replace(candidate);
        drop(previous);
        Poll::Pending
    }
}

/// Deterministic faults; paused directions retain bounded messages until resumed.
#[derive(Clone)]
pub struct MemoryControl {
    endpoints: [Weak<Endpoint>; 2],
}
impl MemoryControl {
    pub fn pause(&self, direction: usize, paused: bool) {
        if let Some(endpoint) = self.endpoints.get(direction).and_then(Weak::upgrade) {
            endpoint.paused.set(paused);
            if !paused {
                endpoint.schedule();
            }
        }
    }
    /// Silent loss leaves logical close to the shared liveness timer.
    pub fn sever(&self) {
        for endpoint in self.endpoints.iter().filter_map(Weak::upgrade) {
            endpoint.severed.set(true);
            endpoint.messages.borrow_mut().clear();
            endpoint.bytes.set(0);
        }
    }
    pub fn queued_bytes(&self, direction: usize) -> usize {
        self.endpoints
            .get(direction)
            .and_then(Weak::upgrade)
            .map_or(0, |endpoint| endpoint.bytes.get())
    }
}

/// Creates a byte-detached FIFO boundary; both admission and receive storage are bounded.
///
/// # Errors
/// Refuses zero/inconsistent budgets before allocating endpoints.
///
/// ```
/// use std::rc::Rc;
/// use futures::StreamExt;
/// use tembra_core::{ManualDriver, rpc::{memory_link, MemoryLinkOptions, LinkEvent, SendError}};
/// let driver = ManualDriver::new();
/// let options = MemoryLinkOptions { max_message_bytes: 4, queued_bytes: 4, queued_messages: 1 };
/// let (sender, mut receiver, control) = memory_link(driver.clone(), options).unwrap();
/// control.pause(0, true);
/// sender.tx.try_send(Rc::from(&b"data"[..])).unwrap();
/// assert_eq!(sender.tx.try_send(Rc::from(&b"x"[..])), Err(SendError::WouldBlock));
/// control.pause(0, false);
/// assert!(matches!(driver.run_until(receiver.events.next()), Some(LinkEvent::Opened)));
/// assert!(matches!(driver.run_until(receiver.events.next()), Some(LinkEvent::Message(bytes)) if bytes == b"data"));
/// ```
pub fn memory_link(
    driver: Rc<dyn Driver>,
    options: MemoryLinkOptions,
) -> Result<(Link, Link, MemoryControl)> {
    if options.max_message_bytes == 0
        || options.max_message_bytes > super::protocol::MAX_FRAME
        || options.queued_bytes < options.max_message_bytes
        || options.queued_messages == 0
    {
        return Err(RuntimeError::invalid(
            "link_limits",
            "inconsistent memory-link budgets",
        ));
    }
    let endpoint = || {
        Rc::new_cyclic(|weak| Endpoint {
            driver: driver.clone(),
            peer: RefCell::default(),
            self_weak: weak.clone(),
            messages: RefCell::default(),
            bytes: Cell::new(0),
            events: RefCell::new(VecDeque::from([LinkEvent::Opened])),
            received_bytes: Cell::new(0),
            received_count: Cell::new(0),
            waiter: RefCell::default(),
            writable: Cell::new(false),
            paused: Cell::new(false),
            severed: Cell::new(false),
            closed: Cell::new(false),
            scheduled: Cell::new(false),
            options: options.clone(),
        })
    };
    let left = endpoint();
    let right = endpoint();
    *left.peer.borrow_mut() = Rc::downgrade(&right);
    *right.peer.borrow_mut() = Rc::downgrade(&left);
    let control = MemoryControl {
        endpoints: [Rc::downgrade(&left), Rc::downgrade(&right)],
    };
    Ok((
        Link {
            tx: left.clone(),
            events: Events(left).boxed_local(),
        },
        Link {
            tx: right.clone(),
            events: Events(right).boxed_local(),
        },
        control,
    ))
}
