use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context, Poll, Waker},
};

use futures::{Stream, StreamExt};
use gloo_timers::future::TimeoutFuture;
use tembra_core::{
    CancellationToken, ErrorCode, RuntimeError,
    rpc::{CloseReason, Link, LinkEvent, LinkTx, SendError},
};
use wasm_bindgen::{JsCast, closure::Closure};
use web_sys::{CloseEvent, Event, MessageEvent, WebSocket};

use crate::boundary::text;

/// Local browser storage limits; these do not grant authority to the peer.
#[derive(Clone)]
pub struct BrowserLinkOptions {
    pub max_frame_bytes: usize,
    pub max_queue_bytes: usize,
    pub max_queue_frames: usize,
}

impl Default for BrowserLinkOptions {
    fn default() -> Self {
        Self {
            max_frame_bytes: 1_048_576,
            max_queue_bytes: 4_194_304,
            max_queue_frames: 256,
        }
    }
}

#[derive(Default)]
struct Inbox {
    queue: VecDeque<LinkEvent>,
    bytes: usize,
    frames: usize,
    writable: bool,
    waker: Option<Waker>,
}

struct Socket {
    socket: WebSocket,
    options: BrowserLinkOptions,
    inbox: RefCell<Inbox>,
    ended: Cell<bool>,
    sent_frames: Cell<usize>,
    polling: Cell<bool>,
    stopped: CancellationToken,
    listeners: RefCell<Option<Listeners>>,
    weak: Weak<Socket>,
}

struct Listeners {
    _open: Closure<dyn FnMut(Event)>,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _error: Closure<dyn FnMut(Event)>,
    _close: Closure<dyn FnMut(CloseEvent)>,
}

impl Socket {
    fn push(&self, event: LinkEvent) {
        let wake = {
            let mut inbox = self.inbox.borrow_mut();
            if matches!(event, LinkEvent::Writable) {
                if inbox.writable {
                    return;
                }
                inbox.writable = true;
            }
            if let LinkEvent::Message(bytes) = &event {
                inbox.bytes += bytes.len();
                inbox.frames += 1;
            }
            inbox.queue.push_back(event);
            inbox.waker.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    fn finish(&self, reason: CloseReason) {
        if self.ended.replace(true) {
            return;
        }
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onerror(None);
        self.socket.set_onclose(None);
        self.stopped.cancel(reason.error.clone());
        let (code, message) = if reason.error.code == ErrorCode::UpgradeRequired {
            (4001, "upgrade required")
        } else if matches!(
            reason.error.code,
            ErrorCode::Invalid | ErrorCode::Forbidden | ErrorCode::ContractMismatch
        ) {
            // Browser close() permits 1000 or application codes 3000..4999.
            (4008, "policy")
        } else {
            (1000, "closed")
        };
        let _ = self.socket.close_with_code_and_reason(code, message);
        self.push(LinkEvent::Closed(reason));
        // A listener can initiate closure itself. Dispose its closure on a later microtask,
        // after its current call has returned; handlers have already been detached.
        let listeners = self.listeners.borrow_mut().take();
        wasm_bindgen_futures::spawn_local(async move {
            drop(listeners);
        });
    }

    fn watch_writable(&self) {
        if self.polling.replace(true) || self.ended.get() {
            return;
        }
        let weak = self.weak.clone();
        let stop = self.stopped.clone();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                if let futures::future::Either::Right(_) =
                    futures::future::select(TimeoutFuture::new(10), stop.cancelled()).await
                {
                    return;
                }
                let Some(socket) = weak.upgrade() else {
                    return;
                };
                if socket.ended.get() {
                    return;
                }
                let bytes = socket.socket.buffered_amount() as usize;
                if bytes == 0 {
                    socket.sent_frames.set(0);
                }
                if socket.socket.ready_state() == WebSocket::OPEN
                    && bytes < socket.options.max_queue_bytes
                    && socket.sent_frames.get() < socket.options.max_queue_frames
                {
                    socket.polling.set(false);
                    socket.push(LinkEvent::Writable);
                    return;
                }
            }
        });
    }
}

impl LinkTx for Socket {
    fn try_send(&self, bytes: Rc<[u8]>) -> Result<(), SendError> {
        if self.ended.get() {
            return Err(SendError::Closed);
        }
        if bytes.len() > self.options.max_frame_bytes {
            return Err(SendError::TooLarge);
        }
        if self.socket.ready_state() != WebSocket::OPEN {
            if self.socket.ready_state() == WebSocket::CONNECTING {
                return Err(SendError::WouldBlock);
            }
            return Err(SendError::Closed);
        }
        let queued = self.socket.buffered_amount() as usize;
        if queued == 0 {
            self.sent_frames.set(0);
        }
        if bytes.len() > self.options.max_queue_bytes.saturating_sub(queued)
            || self.sent_frames.get() >= self.options.max_queue_frames
        {
            self.watch_writable();
            return Err(SendError::WouldBlock);
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            self.finish(CloseReason::new(RuntimeError::invalid(
                "json_utf8",
                "invalid outbound UTF-8",
            )));
            return Err(SendError::Closed);
        };
        if self.socket.send_with_str(text).is_err() {
            self.finish(CloseReason::new(RuntimeError::unavailable(
                "websocket_send",
                "WebSocket send failed",
            )));
            return Err(SendError::Closed);
        }
        self.sent_frames.set(self.sent_frames.get() + 1);
        Ok(())
    }

    fn close(&self, reason: CloseReason) {
        self.finish(reason);
    }
}

struct Events {
    socket: Rc<Socket>,
    finished: bool,
}
impl Stream for Events {
    type Item = LinkEvent;
    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        let event = {
            let mut inbox = this.socket.inbox.borrow_mut();
            let event = inbox.queue.pop_front();
            if let Some(LinkEvent::Message(bytes)) = &event {
                inbox.bytes -= bytes.len();
                inbox.frames -= 1;
            }
            if matches!(event, Some(LinkEvent::Writable)) {
                inbox.writable = false;
            }
            event
        };
        if let Some(event) = event {
            if matches!(event, LinkEvent::Closed(_)) {
                this.finished = true;
            }
            return Poll::Ready(Some(event));
        }
        let new = context.waker().clone();
        let old = this.socket.inbox.borrow_mut().waker.replace(new);
        drop(old);
        Poll::Pending
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.socket.finish(CloseReason::closed());
    }
}

/// Creates a bounded ordered WebSocket Link. It never parses or reserializes wire JSON in JS.
///
/// # Errors
/// Refuses invalid options/URL or browser socket construction. Successful send is bounded
/// browser-queue admission only; disconnect after admission leaves execution outcome unknown.
pub fn websocket_link(url: &str, options: BrowserLinkOptions) -> Result<Link, RuntimeError> {
    if options.max_frame_bytes == 0
        || options.max_frame_bytes > 1_048_576
        || options.max_queue_bytes < options.max_frame_bytes
        || options.max_queue_frames == 0
    {
        return Err(RuntimeError::invalid(
            "link_limits",
            "inconsistent browser Link limits",
        ));
    }
    let websocket = WebSocket::new(url).map_err(|_| {
        RuntimeError::unavailable("websocket_open", "WebSocket construction failed")
    })?;
    let socket = Rc::new_cyclic(|weak| Socket {
        socket: websocket,
        options,
        inbox: RefCell::default(),
        ended: Cell::new(false),
        sent_frames: Cell::new(0),
        polling: Cell::new(false),
        stopped: CancellationToken::new(),
        listeners: RefCell::default(),
        weak: weak.clone(),
    });
    let weak = Rc::downgrade(&socket);
    let open = Closure::new(move |_: Event| {
        if let Some(socket) = weak.upgrade()
            && !socket.ended.get()
        {
            socket.push(LinkEvent::Opened);
            socket.push(LinkEvent::Writable);
        }
    });
    let weak = Rc::downgrade(&socket);
    let message = Closure::new(move |event: MessageEvent| {
        let Some(socket) = weak.upgrade() else {
            return;
        };
        if socket.ended.get() {
            return;
        }
        let Ok(raw) = text(&event.data()) else {
            socket.finish(CloseReason::new(RuntimeError::invalid(
                "websocket_text",
                "expected valid UTF-16 text",
            )));
            return;
        };
        let fits = {
            let inbox = socket.inbox.borrow();
            raw.len() <= socket.options.max_frame_bytes
                && raw.len() <= socket.options.max_queue_bytes.saturating_sub(inbox.bytes)
                && inbox.frames < socket.options.max_queue_frames
        };
        if !fits {
            socket.finish(CloseReason::new(RuntimeError::unavailable(
                "link_queue_limit",
                "incoming WebSocket queue limit exceeded",
            )));
            return;
        }
        socket.push(LinkEvent::Message(raw.into_bytes()));
    });
    let weak = Rc::downgrade(&socket);
    let error = Closure::new(move |_: Event| {
        if let Some(socket) = weak.upgrade() {
            socket.finish(CloseReason::new(RuntimeError::unavailable(
                "websocket",
                "WebSocket failed",
            )));
        }
    });
    let weak = Rc::downgrade(&socket);
    let close = Closure::new(move |event: CloseEvent| {
        if let Some(socket) = weak.upgrade() {
            let error = match event.code() {
                4001 => RuntimeError::new(
                    ErrorCode::UpgradeRequired,
                    "protocol_version",
                    "provider requires upgraded client code",
                ),
                1008 | 4008 => RuntimeError::new(
                    ErrorCode::Forbidden,
                    "policy",
                    "peer refused connection by policy",
                ),
                _ => RuntimeError::unavailable("websocket_closed", "WebSocket closed"),
            };
            socket.finish(CloseReason::new(error));
        }
    });
    socket
        .socket
        .set_onopen(Some(open.as_ref().unchecked_ref()));
    socket
        .socket
        .set_onmessage(Some(message.as_ref().unchecked_ref()));
    socket
        .socket
        .set_onerror(Some(error.as_ref().unchecked_ref()));
    socket
        .socket
        .set_onclose(Some(close.as_ref().unchecked_ref()));
    *socket.listeners.borrow_mut() = Some(Listeners {
        _open: open,
        _message: message,
        _error: error,
        _close: close,
    });
    Ok(Link {
        tx: socket.clone(),
        events: Events {
            socket,
            finished: false,
        }
        .boxed_local(),
    })
}
