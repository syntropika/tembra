mod dispatch;
mod reload;
mod state;

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::{Rc, Weak},
    time::Duration,
};

use futures::{FutureExt, StreamExt, channel::oneshot, future::LocalBoxFuture};
use serde_json::Value;

use super::{CloseReason, Link, LinkEvent, LinkTx, SendError, protocol::*, remote::RemoteBinding};
use crate::{
    CancellationToken, Context, Driver, Reference, Result, RuntimeError, Subscription,
    host::HostInner,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    AwaitHello,
    Open,
    Closed,
}

#[derive(Clone, Debug)]
pub enum ConnectionProfile {
    AcceptedClient,
    ClientToProvider { expected: String },
    AcceptedChild { expected: String },
    TrustedParent { expected: String },
}
impl ConnectionProfile {
    fn client(&self) -> bool {
        matches!(self, Self::ClientToProvider { .. })
    }
    fn accepts_requests(&self) -> bool {
        !self.client()
    }
    pub(crate) fn trusted(&self) -> bool {
        matches!(
            self,
            Self::AcceptedChild { .. } | Self::TrustedParent { .. }
        )
    }
}

/// Local admission budgets. Peers can lower sending offers but cannot raise these limits.
#[derive(Clone, Debug)]
pub struct RpcLimits {
    pub max_frame_bytes: usize,
    pub queued_bytes: usize,
    pub queued_frames: usize,
    pub max_holds: usize,
    pub max_calls: usize,
    pub max_pending_routes: usize,
    pub max_state_members: usize,
    pub max_streams: usize,
    pub max_unit_bytes: usize,
    pub staging_bytes: usize,
    pub state_window: usize,
    pub state_in_flight: usize,
    pub deferred_bytes: usize,
    pub max_delta_ops: usize,
    pub max_path_segments: usize,
    pub max_imported_roots: usize,
    pub max_root_carriers: usize,
    pub max_ended_roots_per_connection: usize,
    pub max_ended_roots_per_host: usize,
    pub max_revocation_history: usize,
    pub max_revocation_bytes: usize,
    pub recoveries: usize,
    pub yield_frames: usize,
}
impl Default for RpcLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: MAX_FRAME,
            queued_bytes: 4_194_304,
            queued_frames: 256,
            max_holds: 128,
            max_calls: 128,
            max_pending_routes: 64,
            max_state_members: 128,
            max_streams: 512,
            max_unit_bytes: 4_194_304,
            staging_bytes: 8_388_608,
            state_window: 262_144,
            state_in_flight: 8_388_608,
            deferred_bytes: 1_048_576,
            max_delta_ops: 4096,
            max_path_segments: 128,
            max_imported_roots: 256,
            max_root_carriers: 64,
            max_ended_roots_per_connection: 4096,
            max_ended_roots_per_host: 16384,
            max_revocation_history: 4096,
            max_revocation_bytes: 1_048_576,
            recoveries: 3,
            yield_frames: 64,
        }
    }
}
#[derive(Clone, Debug)]
pub struct ConnectionOptions {
    pub limits: RpcLimits,
    pub hello_deadline: Duration,
    pub ping_interval: Duration,
    pub liveness_deadline: Duration,
    pub ack_deadline: Duration,
}
impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            limits: RpcLimits::default(),
            hello_deadline: Duration::from_secs(5),
            ping_interval: Duration::from_secs(10),
            liveness_deadline: Duration::from_secs(30),
            ack_deadline: Duration::from_secs(30),
        }
    }
}
impl ConnectionOptions {
    fn validate(&self) -> Result<()> {
        let limit = &self.limits;
        if [
            limit.max_frame_bytes,
            limit.queued_bytes,
            limit.queued_frames,
            limit.max_holds,
            limit.max_calls,
            limit.max_pending_routes,
            limit.max_state_members,
            limit.max_streams,
            limit.max_unit_bytes,
            limit.staging_bytes,
            limit.state_window,
            limit.state_in_flight,
            limit.deferred_bytes,
            limit.max_delta_ops,
            limit.max_path_segments,
            limit.max_imported_roots,
            limit.max_root_carriers,
            limit.max_ended_roots_per_connection,
            limit.max_ended_roots_per_host,
            limit.max_revocation_history,
            limit.max_revocation_bytes,
            limit.recoveries,
            limit.yield_frames,
        ]
        .iter()
        .any(|value| *value == 0 || (*value as u128) > u128::from(MAX_SAFE))
            || limit.max_frame_bytes > MAX_FRAME
            || limit.queued_bytes < limit.max_frame_bytes
            || limit.staging_bytes < limit.max_unit_bytes
            || limit.state_in_flight < limit.max_frame_bytes
            || limit.state_window > limit.state_in_flight
            || self.hello_deadline.is_zero()
            || self.ping_interval.is_zero()
            || self.liveness_deadline <= self.ping_interval
            || self.ack_deadline.is_zero()
        {
            return Err(RuntimeError::invalid(
                "rpc_limits",
                "inconsistent RPC budgets or deadlines",
            ));
        }
        Ok(())
    }
}

struct RemoteHold {
    binding: Weak<RemoteBinding>,
    attachment: AttachmentId,
}
struct ServedHold {
    reference: Reference,
    request: BindRequest,
    principal: Value,
    unit: Cell<u64>,
    subscriptions: RefCell<Vec<Subscription>>,
    state_on: Cell<bool>,
    ready: Cell<bool>,
    generation: Cell<u64>,
    status_subscription: RefCell<Option<Box<dyn Fn()>>>,
}
impl Drop for ServedHold {
    fn drop(&mut self) {
        if let Some(unsubscribe) = self.status_subscription.get_mut().take() {
            unsubscribe();
        }
    }
}
struct IncomingCall {
    cancellation: CancellationToken,
}
type CallSender = oneshot::Sender<Result<Option<Value>>>;
struct Output {
    bytes: Rc<[u8]>,
}

pub(super) struct ConnectionInner {
    reload: reload::PeerState,
    host: Weak<HostInner>,
    id: ConnectionId,
    profile: ConnectionProfile,
    principal: Value,
    driver: Rc<dyn Driver>,
    tx: Rc<dyn LinkTx>,
    options: ConnectionOptions,
    status: Cell<ConnectionStatus>,
    closed: RefCell<Option<CloseReason>>,
    stopped: CancellationToken,
    opened: CancellationToken,
    hello_sent: Cell<bool>,
    flushing: Cell<bool>,
    output: RefCell<VecDeque<Output>>,
    output_bytes: Cell<usize>,
    peer_frame: Cell<usize>,
    peer_unit: Cell<usize>,
    peer_window: Cell<usize>,
    next_hold: Cell<u64>,
    next_call: Cell<u64>,
    next_stream: Cell<u64>,
    next_ping: Cell<u64>,
    last_seen: Cell<Duration>,
    last_ping: Cell<Duration>,
    remote: RefCell<HashMap<HoldId, RemoteHold>>,
    served: RefCell<HashMap<HoldId, Rc<ServedHold>>>,
    received_hold: Cell<u64>,
    received_call: Cell<u64>,
    calls: RefCell<HashMap<CallId, CallSender>>,
    incoming: RefCell<HashMap<CallId, IncomingCall>>,
    streams: RefCell<HashMap<StateKey, state::Feed>>,
    state_bytes: Cell<usize>,
    last_feed: RefCell<Option<StateKey>>,
    revoked: RefCell<HashMap<(String, String), u64>>,
}

/// A cloneable connection handle. Closing is logical termination, not physical handler settlement.
#[derive(Clone)]
pub struct Connection {
    pub(super) inner: Rc<ConnectionInner>,
}

impl Connection {
    pub(crate) fn new(
        host: Weak<HostInner>,
        id: ConnectionId,
        link: Link,
        profile: ConnectionProfile,
        principal: Value,
        options: ConnectionOptions,
    ) -> Result<Self> {
        options.validate()?;
        let owner = host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("host", "host no longer exists"))?;
        let driver = owner.driver.clone();
        let now = driver.now();
        let connection = Self {
            inner: Rc::new(ConnectionInner {
                reload: reload::PeerState::default(),
                host,
                id,
                profile,
                principal,
                driver: driver.clone(),
                tx: link.tx,
                peer_frame: Cell::new(options.limits.max_frame_bytes),
                peer_unit: Cell::new(options.limits.max_unit_bytes),
                peer_window: Cell::new(options.limits.state_window),
                options,
                status: Cell::new(ConnectionStatus::AwaitHello),
                closed: RefCell::default(),
                stopped: CancellationToken::new(),
                opened: CancellationToken::new(),
                hello_sent: Cell::new(false),
                flushing: Cell::new(false),
                output: RefCell::default(),
                output_bytes: Cell::new(0),
                next_hold: Cell::new(0),
                next_call: Cell::new(0),
                next_stream: Cell::new(0),
                next_ping: Cell::new(0),
                last_seen: Cell::new(now),
                last_ping: Cell::new(now),
                remote: RefCell::default(),
                served: RefCell::default(),
                received_hold: Cell::new(0),
                received_call: Cell::new(0),
                calls: RefCell::default(),
                incoming: RefCell::default(),
                streams: RefCell::default(),
                state_bytes: Cell::new(0),
                last_feed: RefCell::default(),
                revoked: RefCell::default(),
            }),
        };
        let weak = Rc::downgrade(&connection.inner);
        driver.spawn(Box::pin(async move {
            let mut events = link.events;
            let mut count = 0;
            loop {
                let stopped = match weak.upgrade() {
                    Some(inner) => inner.stopped.clone(),
                    None => break,
                };
                let next = events.next().boxed_local();
                let event = match futures::future::select(next, stopped.cancelled()).await {
                    futures::future::Either::Left((event, _)) => event,
                    futures::future::Either::Right(_) => break,
                };
                let Some(inner) = weak.upgrade() else { break };
                let connection = Self { inner };
                match event {
                    Some(LinkEvent::Opened) => {
                        if let Err(error) = connection.hello() {
                            connection.fail(error);
                        }
                    }
                    Some(LinkEvent::Message(bytes)) => connection.message(&bytes),
                    Some(LinkEvent::Writable) => connection.flush(),
                    Some(LinkEvent::Closed(reason)) => {
                        connection.close(reason);
                        break;
                    }
                    None => {
                        connection.close(CloseReason::closed());
                        break;
                    }
                }
                count += 1;
                if count >= connection.inner.options.limits.yield_frames {
                    count = 0;
                    drop(connection);
                    yield_once().await;
                }
            }
        }));
        let weak = Rc::downgrade(&connection.inner);
        driver.spawn(Box::pin(async move {
            loop {
                let Some(inner) = weak.upgrade() else { break };
                if inner.status.get() == ConnectionStatus::Closed {
                    break;
                }
                let delay = inner
                    .options
                    .ping_interval
                    .min(inner.options.hello_deadline)
                    .min(inner.options.ack_deadline);
                let stopped = inner.stopped.clone();
                let sleep = inner.driver.sleep(delay);
                drop(inner);
                if matches!(
                    futures::future::select(sleep, stopped.cancelled()).await,
                    futures::future::Either::Right(_)
                ) {
                    break;
                }
                if let Some(inner) = weak.upgrade() {
                    Self { inner }.tick();
                }
            }
        }));
        Ok(connection)
    }
    pub fn id(&self) -> ConnectionId {
        self.inner.id
    }
    pub(crate) fn authority_realm(&self) -> ConnectionId {
        self.id()
    }
    pub(crate) fn limits(&self) -> &RpcLimits {
        &self.inner.options.limits
    }
    pub(crate) fn has_hold_capacity(&self) -> bool {
        self.inner.remote.borrow().len() < self.inner.options.limits.max_holds
    }
    pub(crate) fn belongs_to(&self, host_id: u64) -> bool {
        self.inner
            .host
            .upgrade()
            .is_some_and(|host| host.id == host_id)
    }
    pub(crate) fn cancel_ingress(&self, reason: RuntimeError) {
        let tokens: Vec<_> = self
            .inner
            .incoming
            .borrow()
            .values()
            .map(|call| call.cancellation.clone())
            .collect();
        for token in tokens {
            token.cancel(reason.clone());
        }
    }
    pub(crate) fn trusted(&self) -> bool {
        self.inner.profile.trusted()
    }
    pub(crate) fn profile(&self) -> &ConnectionProfile {
        &self.inner.profile
    }
    pub fn status(&self) -> ConnectionStatus {
        self.inner.status.get()
    }
    /// Waits for a compatible hello, without acquiring an object.
    ///
    /// # Errors
    /// Returns the structured close reason, including protocol upgrade failure.
    pub fn ready(&self) -> LocalBoxFuture<'static, Result<()>> {
        let connection = self.clone();
        async move {
            loop {
                match connection.status() {
                    ConnectionStatus::Open
                        if !matches!(
                            connection.profile(),
                            ConnectionProfile::AcceptedChild { .. }
                        ) || connection.reload_synced() =>
                    {
                        return Ok(());
                    }
                    ConnectionStatus::Open => {}
                    ConnectionStatus::Closed => {
                        return Err(connection
                            .inner
                            .closed
                            .borrow()
                            .as_ref()
                            .expect("closed reason")
                            .error
                            .clone());
                    }
                    ConnectionStatus::AwaitHello => {}
                }
                let _ = futures::future::select(
                    connection.inner.opened.cancelled(),
                    connection.inner.stopped.cancelled(),
                )
                .await;
            }
        }
        .boxed_local()
    }
    pub fn closed(&self) -> LocalBoxFuture<'static, CloseReason> {
        let connection = self.clone();
        async move {
            loop {
                if let Some(reason) = connection.inner.closed.borrow().clone() {
                    return reason;
                }
                let _ = connection.inner.stopped.cancelled().await;
            }
        }
        .boxed_local()
    }
    /// Logically closes this link once and releases its served holds and queued frames.
    ///
    /// Every admitted outgoing call receives `unavailable/unknown_outcome`, conservatively
    /// including calls whose bytes may still be queued locally. Calls are never replayed.
    /// Transient close returns remote bindings to Loading with cached state; permanent
    /// upgrade or policy refusal terminates them. Incoming job contexts are signalled, but
    /// their driver-owned callbacks and physical pins remain until actual settlement.
    /// This method does not await user handlers or resource cleanup; cancellation alone
    /// does not prove physical termination. Repeated close has no additional effects.
    pub fn close(&self, reason: CloseReason) {
        if self.inner.status.replace(ConnectionStatus::Closed) == ConnectionStatus::Closed {
            return;
        }
        *self.inner.closed.borrow_mut() = Some(reason.clone());
        self.inner.stopped.cancel(reason.error.clone());
        self.reload_closed(reason.error.clone());
        let calls = std::mem::take(&mut *self.inner.calls.borrow_mut());
        for sender in calls.into_values() {
            let _ = sender.send(Err(RuntimeError::unavailable(
                "unknown_outcome",
                "link closed after call admission",
            )));
        }
        let remote = std::mem::take(&mut *self.inner.remote.borrow_mut());
        for entry in remote.into_values() {
            if let Some(binding) = entry.binding.upgrade() {
                binding.rpc_detached(entry.attachment, reason.error.clone());
            }
        }
        let served = std::mem::take(&mut *self.inner.served.borrow_mut());
        for hold in served.into_values() {
            hold.reference.release();
        }
        let cancellations: Vec<_> = self
            .inner
            .incoming
            .borrow()
            .values()
            .map(|job| job.cancellation.clone())
            .collect();
        for token in cancellations {
            token.cancel(reason.error.clone());
        }
        self.inner.streams.borrow_mut().clear();
        self.inner.state_bytes.set(0);
        self.inner.last_feed.borrow_mut().take();
        self.inner.output.borrow_mut().clear();
        self.inner.revoked.borrow_mut().clear();
        self.inner.output_bytes.set(0);
        if let Some(host) = self.inner.host.upgrade() {
            host.rpc_link_closed(self.id(), reason.error.clone());
        }
        self.inner.tx.close(reason);
    }
    fn fail(&self, error: RuntimeError) {
        self.close(CloseReason::new(error));
    }
    fn require_open(&self) -> Result<()> {
        if self.status() == ConnectionStatus::Open {
            Ok(())
        } else {
            Err(RuntimeError::unavailable(
                "connection",
                "connection is not open",
            ))
        }
    }
    fn next(counter: &Cell<u64>) -> Result<u64> {
        let value = counter
            .get()
            .checked_add(1)
            .filter(|id| *id <= MAX_SAFE)
            .ok_or_else(|| RuntimeError::unavailable("rpc_id", "identifier space exhausted"))?;
        counter.set(value);
        Ok(value)
    }
    fn send(&self, frame: Frame) -> Result<()> {
        let mut observation = crate::observability::start(
            &self
                .inner
                .host
                .upgrade()
                .and_then(|host| host.observations.clone()),
            crate::observability::Operation::RpcSend,
            "enqueue",
            None,
            None,
            None,
        );
        let result = (|| {
            if self.status() == ConnectionStatus::Closed {
                return Err(RuntimeError::unavailable("closed", "connection closed"));
            }
            let bytes = encode(&frame, self.inner.peer_frame.get())?;
            self.enqueue(bytes)
        })();
        if let Some(observation) = &mut observation {
            observation.result(&result);
        }
        result
    }
    fn output_fits(&self, bytes: usize, frames: usize, state: bool) -> bool {
        let limits = &self.inner.options.limits;
        let (byte_limit, frame_limit) = if state {
            // One maximum control frame and bounded small-frame headroom remain available.
            // A tiny configured queue can deliberately have no state admission capacity.
            let reserve = self.inner.peer_frame.get().saturating_add(16_384);
            (
                limits.queued_bytes.saturating_sub(reserve),
                limits.queued_frames.saturating_sub(16),
            )
        } else {
            (limits.queued_bytes, limits.queued_frames)
        };
        frames <= frame_limit.saturating_sub(self.inner.output.borrow().len())
            && bytes <= byte_limit.saturating_sub(self.inner.output_bytes.get())
    }
    fn append_output(&self, batch: Vec<Rc<[u8]>>) {
        let bytes: usize = batch.iter().map(|bytes| bytes.len()).sum();
        self.inner
            .output_bytes
            .set(self.inner.output_bytes.get() + bytes);
        self.inner
            .output
            .borrow_mut()
            .extend(batch.into_iter().map(|bytes| Output { bytes }));
    }
    fn enqueue(&self, bytes: Rc<[u8]>) -> Result<()> {
        if !self.output_fits(bytes.len(), 1, false) {
            return Err(RuntimeError::unavailable(
                "output_limit",
                "bounded connection output is full",
            ));
        }
        self.append_output(vec![bytes]);
        self.flush();
        Ok(())
    }
    fn flush(&self) {
        if self.inner.flushing.replace(true) {
            return;
        }
        struct FlushGuard<'a>(&'a Cell<bool>);
        impl Drop for FlushGuard<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _guard = FlushGuard(&self.inner.flushing);
        loop {
            let bytes = self
                .inner
                .output
                .borrow()
                .front()
                .map(|output| output.bytes.clone());
            let Some(bytes) = bytes else {
                if let Err(error) = self.flush_feeds() {
                    self.fail(error);
                    return;
                }
                if self.inner.output.borrow().is_empty() {
                    break;
                }
                continue;
            };
            match self.inner.tx.try_send(bytes.clone()) {
                Ok(()) => {
                    self.inner.output.borrow_mut().pop_front();
                    self.inner
                        .output_bytes
                        .set(self.inner.output_bytes.get().saturating_sub(bytes.len()));
                }
                Err(SendError::WouldBlock) => break,
                Err(error) => {
                    self.fail(RuntimeError::unavailable(
                        "transport",
                        format!("transport admission failed: {error:?}"),
                    ));
                    break;
                }
            }
        }
    }
    fn hello(&self) -> Result<()> {
        if self.inner.hello_sent.replace(true) {
            return Err(RuntimeError::invalid(
                "rpc_hello",
                "transport opened more than once",
            ));
        }
        let peer = if self.inner.profile.client() {
            PeerHello::Client {}
        } else {
            PeerHello::Host {
                identity: self
                    .inner
                    .host
                    .upgrade()
                    .ok_or_else(|| RuntimeError::unavailable("host", "host ended"))?
                    .rpc_identity()?,
                code_version: PositiveId::new(
                    self.inner
                        .host
                        .upgrade()
                        .ok_or_else(|| RuntimeError::unavailable("closed", "Host ended"))?
                        .reload_current_version(),
                )?,
            }
        };
        let limits = &self.inner.options.limits;
        self.send(Frame::Hello {
            hello: Hello {
                protocol: "tembra-rpc".into(),
                version: SafeU64::new(1)?,
                descriptor_version: SafeU64::new(1)?,
                delta_codec: SafeU64::new(1)?,
                peer,
                receive: ReceiveOffer {
                    max_message_bytes: SafeU64::new(limits.max_frame_bytes as u64)?,
                    max_unit_bytes: SafeU64::new(limits.max_unit_bytes as u64)?,
                    state_window_bytes: SafeU64::new(limits.state_window as u64)?,
                },
            },
        })
    }
    fn tick(&self) {
        let now = self.inner.driver.now();
        if self.status() == ConnectionStatus::AwaitHello
            && now.saturating_sub(self.inner.last_seen.get()) >= self.inner.options.hello_deadline
        {
            self.fail(RuntimeError::unavailable(
                "hello_timeout",
                "compatible hello deadline elapsed",
            ));
            return;
        }
        if self.status() == ConnectionStatus::Open
            && now.saturating_sub(self.inner.last_seen.get())
                >= self.inner.options.liveness_deadline
        {
            self.fail(RuntimeError::unavailable(
                "liveness",
                "peer stopped responding",
            ));
            return;
        }
        if self.status() == ConnectionStatus::Open
            && now.saturating_sub(self.inner.last_ping.get()) >= self.inner.options.ping_interval
        {
            self.inner.last_ping.set(now);
            match Self::next(&self.inner.next_ping)
                .and_then(HoldId::new)
                .and_then(|nonce| self.send(Frame::Ping { nonce }))
            {
                Ok(()) => {}
                Err(error) => self.fail(error),
            }
        }
        self.check_ack_deadlines();
    }
}
async fn yield_once() {
    let mut yielded = false;
    futures::future::poll_fn(move |context| {
        if std::mem::replace(&mut yielded, true) {
            std::task::Poll::Ready(())
        } else {
            context.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}
