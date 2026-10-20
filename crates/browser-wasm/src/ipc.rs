//! Bounded opaque-frame Link for platform IPC drivers. RPC remains entirely in core.
use futures::{Stream, StreamExt};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};
use tembra_core::{
    Result, RuntimeError,
    rpc::{CloseReason, Link, LinkEvent, LinkTx, SendError},
};

pub(crate) struct Endpoint {
    outbound: RefCell<VecDeque<String>>,
    incoming: RefCell<VecDeque<LinkEvent>>,
    outbound_bytes: Cell<usize>,
    incoming_bytes: Cell<usize>,
    waiter: RefCell<Option<Waker>>,
    ended: Cell<bool>,
}
impl Endpoint {
    pub fn new() -> (Rc<Self>, Link) {
        let endpoint = Rc::new(Self {
            outbound: RefCell::default(),
            incoming: RefCell::new(VecDeque::from([LinkEvent::Opened])),
            outbound_bytes: Cell::new(0),
            incoming_bytes: Cell::new(0),
            waiter: RefCell::default(),
            ended: Cell::new(false),
        });
        let link = Link {
            tx: endpoint.clone(),
            events: Events(endpoint.clone()).boxed_local(),
        };
        (endpoint, link)
    }
    fn event(&self, event: LinkEvent) {
        self.incoming.borrow_mut().push_back(event);
        if let Some(w) = self.waiter.borrow_mut().take() {
            w.wake();
        }
    }
    pub fn receive(&self, frame: String) -> Result<()> {
        if self.ended.get() {
            return Err(RuntimeError::unavailable(
                "ipc_closed",
                "IPC link is closed",
            ));
        }
        if frame.len() > 1_048_576
            || self.incoming.borrow().len() >= 256
            || self.incoming_bytes.get() + frame.len() > 4_194_304
        {
            self.close(CloseReason::new(RuntimeError::unavailable(
                "ipc_limit",
                "IPC receive budget exceeded",
            )));
            return Err(RuntimeError::unavailable(
                "ipc_limit",
                "IPC receive budget exceeded",
            ));
        }
        self.incoming_bytes
            .set(self.incoming_bytes.get() + frame.len());
        self.event(LinkEvent::Message(frame.into_bytes()));
        Ok(())
    }
    pub fn drain(&self) -> Vec<String> {
        let frames = self.outbound.borrow_mut().drain(..).collect();
        let was_queued = self.outbound_bytes.replace(0) > 0;
        if was_queued && !self.ended.get() {
            self.event(LinkEvent::Writable);
        }
        frames
    }
}
impl LinkTx for Endpoint {
    fn try_send(&self, bytes: Rc<[u8]>) -> std::result::Result<(), SendError> {
        if self.ended.get() {
            return Err(SendError::Closed);
        }
        if bytes.len() > 1_048_576 {
            return Err(SendError::TooLarge);
        }
        if self.outbound.borrow().len() >= 256
            || self.outbound_bytes.get() + bytes.len() > 4_194_304
        {
            return Err(SendError::WouldBlock);
        }
        let frame = std::str::from_utf8(&bytes)
            .map_err(|_| SendError::Closed)?
            .to_owned();
        self.outbound_bytes
            .set(self.outbound_bytes.get() + bytes.len());
        self.outbound.borrow_mut().push_back(frame);
        Ok(())
    }
    fn close(&self, reason: CloseReason) {
        if !self.ended.replace(true) {
            self.outbound.borrow_mut().clear();
            self.outbound_bytes.set(0);
            self.event(LinkEvent::Closed(reason));
        }
    }
}
struct Events(Rc<Endpoint>);
impl Stream for Events {
    type Item = LinkEvent;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<LinkEvent>> {
        let event = self.0.incoming.borrow_mut().pop_front();
        if let Some(event) = event {
            if let LinkEvent::Message(bytes) = &event {
                self.0
                    .incoming_bytes
                    .set(self.0.incoming_bytes.get() - bytes.len());
            }
            return Poll::Ready(Some(event));
        }
        if self.0.ended.get() {
            return Poll::Ready(None);
        }
        self.0.waiter.borrow_mut().replace(cx.waker().clone());
        Poll::Pending
    }
}
impl Drop for Events {
    fn drop(&mut self) {
        self.0.close(CloseReason::closed());
    }
}
