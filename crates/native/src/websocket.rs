use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
    time::Duration,
};

use futures::{
    FutureExt, SinkExt, Stream, StreamExt,
    future::{Either, poll_fn, select},
};
use serde_json::Value;
use tembra_core::{
    Driver, ErrorCode, Result, RuntimeError,
    rpc::{CloseReason, Link, LinkEvent, LinkTx, SendError},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        protocol::{CloseFrame, WebSocketConfig, frame::coding::CloseCode},
    },
};

use crate::TokioDriver;

/// Explicit byte/frame budgets for one native WebSocket adapter.
///
/// Options are checked before attaching a socket. Both normal input and output
/// must fit one maximum-sized frame. The pre-auth queue has independent limits.
#[derive(Clone, Debug)]
pub struct WebSocketOptions {
    pub max_frame_bytes: usize,
    pub incoming_bytes: usize,
    pub incoming_frames: usize,
    pub outgoing_bytes: usize,
    pub outgoing_frames: usize,
    pub pre_auth_bytes: usize,
    pub pre_auth_frames: usize,
    pub auth_timeout: Duration,
}

impl Default for WebSocketOptions {
    fn default() -> Self {
        Self {
            max_frame_bytes: 1_048_576,
            incoming_bytes: 4_194_304,
            incoming_frames: 256,
            outgoing_bytes: 4_194_304,
            outgoing_frames: 256,
            pre_auth_bytes: 1_048_576,
            pre_auth_frames: 32,
            auth_timeout: Duration::from_secs(5),
        }
    }
}

impl WebSocketOptions {
    fn validate(&self) -> Result<()> {
        if self.max_frame_bytes == 0
            || self.max_frame_bytes > 1_048_576
            || self.incoming_bytes < self.max_frame_bytes
            || self.outgoing_bytes < self.max_frame_bytes
            || self.incoming_frames == 0
            || self.outgoing_frames == 0
            || self.pre_auth_bytes == 0
            || self.pre_auth_frames == 0
            || self.pre_auth_bytes > self.incoming_bytes
            || self.pre_auth_frames > self.incoming_frames
            || self.auth_timeout.is_zero()
            || self
                .outgoing_bytes
                .checked_add(self.max_frame_bytes)
                .is_none()
        {
            return Err(RuntimeError::invalid(
                "transport_limits",
                "inconsistent WebSocket budgets",
            ));
        }
        Ok(())
    }

    /// Configures message, fragment and write-buffer limits before the upgrade.
    ///
    /// # Errors
    /// Refuses zero, overflowing or parser-incompatible budgets.
    pub fn protocol_config(&self) -> Result<WebSocketConfig> {
        self.validate()?;
        Ok(WebSocketConfig::default()
            .max_message_size(Some(self.max_frame_bytes))
            .max_frame_size(Some(self.max_frame_bytes))
            .write_buffer_size(0)
            .max_write_buffer_size(self.outgoing_bytes + self.max_frame_bytes))
    }
}

/// Converts an already upgraded, bounded WebSocket into the shared byte Link.
///
/// No typed protocol frames bypass the shared Rust decoder. The embedding owns
/// authentication, HTTP/TLS and driving the supplied TokioDriver local set.
/// Incoming close codes 1008/4008 preserve the coarse Forbidden/policy category;
/// 4001 preserves UpgradeRequired/protocol_version. Close codes do not reconstruct
/// the peer's original RuntimeError or trust its free-form diagnostic text.
pub struct WebSocketLink;

impl WebSocketLink {
    /// Attaches an authenticated socket and schedules its local reader/writer.
    ///
    /// # Errors
    /// Refuses invalid budgets or a socket upgraded without restrictive message,
    /// frame and write-buffer settings. Use `options.protocol_config()` at upgrade.
    pub fn attach<S>(
        socket: WebSocketStream<S>,
        driver: Rc<TokioDriver>,
        options: WebSocketOptions,
    ) -> Result<Link>
    where
        S: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        Self::start(socket, driver, options, true).map(|(link, _)| link)
    }

    /// Reads into a bounded private queue while embedding authentication runs.
    ///
    /// No Link or Opened event escapes until authentication succeeds. Early text
    /// messages are retained in FIFO order; overflow, disconnect or timeout drops
    /// the socket and returns an error without an application connection/claim.
    /// The losing authentication future is dropped, so it must be cancellation-safe.
    ///
    /// # Errors
    /// Returns the authentication error, unavailable/auth_timeout, a transport
    /// failure or invalid configuration. The principal is returned unchanged;
    /// the embedding passes it to Host::accept_client, never reads it from frames.
    pub async fn authenticate<S, F>(
        socket: WebSocketStream<S>,
        driver: Rc<TokioDriver>,
        options: WebSocketOptions,
        authentication: F,
    ) -> Result<(Link, Value)>
    where
        S: AsyncRead + AsyncWrite + Unpin + 'static,
        F: Future<Output = Result<Value>>,
    {
        let timeout = options.auth_timeout;
        let (link, state) = Self::start(socket, driver, options, false)?;
        let authentication = Box::pin(authentication);
        let stopped = state.closed(Waiter::Auth).boxed_local();
        let admitted = async {
            match select(authentication, stopped).await {
                Either::Left((result, _)) => result,
                Either::Right((reason, _)) => Err(reason.error),
            }
        };
        let result = match tokio::time::timeout(timeout, admitted).await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::unavailable(
                "auth_timeout",
                "WebSocket authentication deadline expired",
            )),
        };
        match result {
            Ok(principal) => {
                if let Some(reason) = state.reason() {
                    return Err(reason.error);
                }
                state.authenticated.set(true);
                state.opened.set(true);
                wake(&state.events_waker);
                wake(&state.capacity_waker);
                Ok((link, principal))
            }
            Err(error) => {
                state.finish(CloseReason::new(error.clone()));
                Err(error)
            }
        }
    }

    fn start<S>(
        socket: WebSocketStream<S>,
        driver: Rc<TokioDriver>,
        options: WebSocketOptions,
        authenticated: bool,
    ) -> Result<(Link, Rc<Shared>)>
    where
        S: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        options.validate()?;
        let config = socket.get_config();
        if config
            .max_message_size
            .is_none_or(|limit| limit > options.max_frame_bytes)
            || config
                .max_frame_size
                .is_none_or(|limit| limit > options.max_frame_bytes)
            || config.max_write_buffer_size > options.outgoing_bytes + options.max_frame_bytes
        {
            return Err(RuntimeError::invalid(
                "transport_limits",
                "WebSocket must be upgraded with bounded protocol_config",
            ));
        }
        let state = Rc::new(Shared::new(options, authenticated));
        let (sink, stream) = socket.split();
        driver.spawn(read_loop(stream, state.clone()).boxed_local());
        driver.spawn(write_loop(sink, state.clone()).boxed_local());
        let link = Link {
            tx: Rc::new(Sender(state.clone())),
            events: Box::pin(Events {
                state: state.clone(),
                terminal: false,
            }),
        };
        Ok((link, state))
    }
}

#[derive(Clone, Copy)]
enum Waiter {
    Read,
    Write,
    Auth,
}

struct Shared {
    options: WebSocketOptions,
    authenticated: Cell<bool>,
    opened: Cell<bool>,
    writable: Cell<bool>,
    flush: Cell<bool>,
    incoming: RefCell<VecDeque<Vec<u8>>>,
    incoming_bytes: Cell<usize>,
    outgoing: RefCell<VecDeque<Rc<[u8]>>>,
    outgoing_bytes: Cell<usize>,
    outgoing_frames: Cell<usize>,
    reason: RefCell<Option<CloseReason>>,
    events_waker: RefCell<Option<Waker>>,
    output_waker: RefCell<Option<Waker>>,
    capacity_waker: RefCell<Option<Waker>>,
    read_stop: RefCell<Option<Waker>>,
    write_stop: RefCell<Option<Waker>>,
    auth_stop: RefCell<Option<Waker>>,
}

impl Shared {
    fn new(options: WebSocketOptions, authenticated: bool) -> Self {
        Self {
            options,
            authenticated: Cell::new(authenticated),
            opened: Cell::new(authenticated),
            writable: Cell::new(false),
            flush: Cell::new(false),
            incoming: RefCell::default(),
            incoming_bytes: Cell::new(0),
            outgoing: RefCell::default(),
            outgoing_bytes: Cell::new(0),
            outgoing_frames: Cell::new(0),
            reason: RefCell::default(),
            events_waker: RefCell::default(),
            output_waker: RefCell::default(),
            capacity_waker: RefCell::default(),
            read_stop: RefCell::default(),
            write_stop: RefCell::default(),
            auth_stop: RefCell::default(),
        }
    }

    fn reason(&self) -> Option<CloseReason> {
        self.reason.borrow().clone()
    }

    fn finish(&self, reason: CloseReason) {
        if self.reason.borrow().is_some() {
            return;
        }
        *self.reason.borrow_mut() = Some(reason);
        let abandoned = self.outgoing.take();
        self.outgoing_bytes.set(0);
        self.outgoing_frames.set(0);
        drop(abandoned);
        for slot in [
            &self.events_waker,
            &self.output_waker,
            &self.capacity_waker,
            &self.read_stop,
            &self.write_stop,
            &self.auth_stop,
        ] {
            wake(slot);
        }
    }

    fn slot(&self, waiter: Waiter) -> &RefCell<Option<Waker>> {
        match waiter {
            Waiter::Read => &self.read_stop,
            Waiter::Write => &self.write_stop,
            Waiter::Auth => &self.auth_stop,
        }
    }

    async fn closed(&self, waiter: Waiter) -> CloseReason {
        poll_fn(|cx| {
            if let Some(reason) = self.reason() {
                return Poll::Ready(reason);
            }
            register(self.slot(waiter), cx.waker());
            Poll::Pending
        })
        .await
    }

    fn has_input_capacity(&self) -> bool {
        self.incoming.borrow().len() < self.options.incoming_frames
            && self.incoming_bytes.get()
                <= self.options.incoming_bytes - self.options.max_frame_bytes
    }
}

fn register(slot: &RefCell<Option<Waker>>, waker: &Waker) {
    let replacement = waker.clone();
    let previous = slot.replace(Some(replacement));
    drop(previous);
}

fn wake(slot: &RefCell<Option<Waker>>) {
    let waker = slot.take();
    if let Some(waker) = waker {
        waker.wake();
    }
}

struct Sender(Rc<Shared>);

impl LinkTx for Sender {
    fn try_send(&self, utf8: Rc<[u8]>) -> std::result::Result<(), SendError> {
        let state = &self.0;
        if state.reason.borrow().is_some() {
            return Err(SendError::Closed);
        }
        if utf8.len() > state.options.max_frame_bytes {
            return Err(SendError::TooLarge);
        }
        if std::str::from_utf8(&utf8).is_err() {
            state.finish(CloseReason::new(RuntimeError::invalid(
                "transport_utf8",
                "outgoing text is not UTF-8",
            )));
            return Err(SendError::Closed);
        }
        if !state.authenticated.get()
            || state.outgoing_frames.get() >= state.options.outgoing_frames
            || utf8.len() > state.options.outgoing_bytes - state.outgoing_bytes.get()
        {
            return Err(SendError::WouldBlock);
        }
        state
            .outgoing_bytes
            .set(state.outgoing_bytes.get() + utf8.len());
        state.outgoing_frames.set(state.outgoing_frames.get() + 1);
        state.outgoing.borrow_mut().push_back(utf8);
        wake(&state.output_waker);
        Ok(())
    }

    fn close(&self, reason: CloseReason) {
        self.0.finish(reason);
    }
}

struct Events {
    state: Rc<Shared>,
    terminal: bool,
}

impl Stream for Events {
    type Item = LinkEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminal {
            return Poll::Ready(None);
        }
        if self.state.opened.replace(false) {
            return Poll::Ready(Some(LinkEvent::Opened));
        }
        if self.state.writable.replace(false) && self.state.reason.borrow().is_none() {
            return Poll::Ready(Some(LinkEvent::Writable));
        }
        let message = self.state.incoming.borrow_mut().pop_front();
        if let Some(message) = message {
            self.state
                .incoming_bytes
                .set(self.state.incoming_bytes.get() - message.len());
            wake(&self.state.capacity_waker);
            return Poll::Ready(Some(LinkEvent::Message(message)));
        }
        if let Some(reason) = self.state.reason() {
            self.terminal = true;
            return Poll::Ready(Some(LinkEvent::Closed(reason)));
        }
        register(&self.state.events_waker, cx.waker());
        Poll::Pending
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.state.finish(CloseReason::closed());
        let abandoned = self.state.incoming.take();
        self.state.incoming_bytes.set(0);
        drop(abandoned);
    }
}

async fn read_loop<S>(mut stream: S, state: Rc<Shared>)
where
    S: Stream<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        if state.authenticated.get() {
            poll_fn(|cx| {
                if state.reason.borrow().is_some() || state.has_input_capacity() {
                    Poll::Ready(())
                } else {
                    register(&state.capacity_waker, cx.waker());
                    Poll::Pending
                }
            })
            .await;
        }
        if state.reason.borrow().is_some() {
            break;
        }
        let next = select(stream.next(), state.closed(Waiter::Read).boxed_local()).await;
        let message = match next {
            Either::Right(_) => break,
            Either::Left((Some(Ok(message)), _)) => message,
            Either::Left((Some(Err(error)), _)) => {
                state.finish(CloseReason::new(RuntimeError::unavailable(
                    "websocket",
                    error.to_string(),
                )));
                break;
            }
            Either::Left((None, _)) => {
                state.finish(CloseReason::closed());
                break;
            }
        };
        match message {
            Message::Text(text) => {
                let bytes = text.as_bytes();
                let (limit_bytes, limit_frames) = if state.authenticated.get() {
                    (state.options.incoming_bytes, state.options.incoming_frames)
                } else {
                    (state.options.pre_auth_bytes, state.options.pre_auth_frames)
                };
                if bytes.len() > state.options.max_frame_bytes
                    || bytes.len() > limit_bytes - state.incoming_bytes.get().min(limit_bytes)
                    || state.incoming.borrow().len() >= limit_frames
                {
                    state.finish(CloseReason::new(RuntimeError::unavailable(
                        "transport_overflow",
                        "incoming WebSocket queue exceeded its budget",
                    )));
                    break;
                }
                state
                    .incoming_bytes
                    .set(state.incoming_bytes.get() + bytes.len());
                state.incoming.borrow_mut().push_back(bytes.to_vec());
                wake(&state.events_waker);
            }
            Message::Binary(_) => {
                state.finish(CloseReason::new(RuntimeError::invalid(
                    "transport_binary",
                    "RPC v1 requires text WebSocket messages",
                )));
                break;
            }
            Message::Ping(_) => {
                state.flush.set(true);
                wake(&state.output_waker);
            }
            Message::Pong(_) => {}
            Message::Close(frame) => {
                let reason = match frame.as_ref().map(|frame| frame.code) {
                    Some(CloseCode::Library(4001)) => CloseReason::new(RuntimeError::new(
                        ErrorCode::UpgradeRequired,
                        "protocol_version",
                        "peer requires a compatible RPC version",
                    )),
                    Some(CloseCode::Policy | CloseCode::Library(4008)) => {
                        CloseReason::new(RuntimeError::new(
                            ErrorCode::Forbidden,
                            "policy",
                            "peer refused connection by policy",
                        ))
                    }
                    _ => CloseReason::closed(),
                };
                state.finish(reason);
                break;
            }
            Message::Frame(_) => {}
        }
    }
}

enum Output {
    Message(Rc<[u8]>),
    Flush,
    Close,
}

async fn write_loop<S>(mut sink: S, state: Rc<Shared>)
where
    S: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    loop {
        let output = poll_fn(|cx| {
            if state.reason.borrow().is_some() {
                return Poll::Ready(Output::Close);
            }
            let message = state.outgoing.borrow_mut().pop_front();
            if let Some(message) = message {
                return Poll::Ready(Output::Message(message));
            }
            if state.flush.replace(false) {
                return Poll::Ready(Output::Flush);
            }
            register(&state.output_waker, cx.waker());
            Poll::Pending
        })
        .await;
        let bytes = match output {
            Output::Close => {
                let code = match state.reason().map(|reason| reason.error.code) {
                    Some(
                        ErrorCode::Invalid | ErrorCode::Forbidden | ErrorCode::ContractMismatch,
                    ) => CloseCode::Policy,
                    Some(ErrorCode::UpgradeRequired) => CloseCode::Library(4001),
                    _ => CloseCode::Normal,
                };
                let frame = CloseFrame {
                    code,
                    reason: "connection closed".into(),
                };
                let _ = tokio::time::timeout(
                    Duration::from_millis(100),
                    sink.send(Message::Close(Some(frame))),
                )
                .await;
                break;
            }
            Output::Flush => None,
            Output::Message(ref bytes) => Some(bytes.len()),
        };
        let write = async {
            match output {
                Output::Message(bytes) => {
                    // LinkTx checked UTF-8 before atomic admission.
                    let text = std::str::from_utf8(&bytes).expect("queued text is UTF-8");
                    sink.send(Message::Text(text.to_owned().into())).await
                }
                Output::Flush => sink.flush().await,
                Output::Close => unreachable!(),
            }
        }
        .boxed_local();
        match select(write, state.closed(Waiter::Write).boxed_local()).await {
            Either::Left((Ok(()), _)) => {
                if let Some(bytes) = bytes.filter(|_| state.reason.borrow().is_none()) {
                    state.outgoing_bytes.set(state.outgoing_bytes.get() - bytes);
                    state.outgoing_frames.set(state.outgoing_frames.get() - 1);
                    state.writable.set(true);
                    wake(&state.events_waker);
                }
            }
            Either::Left((Err(error), _)) => {
                state.finish(CloseReason::new(RuntimeError::unavailable(
                    "websocket",
                    error.to_string(),
                )));
            }
            Either::Right(_) => {}
        }
    }
}
