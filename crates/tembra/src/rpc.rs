//! Optional server for the SDK's existing JSON byte protocol.
//!
//! The embedding authenticates the peer, fixes its principal and supplies a bounded byte sender.
//! There is no socket, reconnect policy or second lifecycle engine here. See [`serve`].
use crate::{Code, Context, Error, Host, Observation, Reference, Result};
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    io::Write,
    rc::{Rc, Weak},
};

const MAX_ID: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Op {
    Describe,
    Get,
    Release,
    Call,
    Cancel,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct Request {
    id: Option<u64>,
    op: Op,
    #[serde(default)]
    payload: Value,
}

type Sender = Rc<dyn Fn(Vec<u8>) -> Result<()>>;
type Policy = Rc<dyn Fn(&str, Option<&[String]>) -> bool>;

/// Per-connection authority and admission limits, fixed when [`serve`] consumes these options.
/// Frame limits bound encoded buffers, not application values or transport-owned memory.
pub struct Options {
    principal: String,
    allow: Policy,
    /// Maximum incoming or outgoing encoded frame bytes (default 65,536).
    /// A limit smaller than a structured error frame can make refusal close the connection.
    pub max_frame: usize,
    /// Maximum concurrently pending requests, including physically unsettled calls (default 64).
    pub max_pending: usize,
    /// Maximum acquired remote claims (default 32).
    pub max_references: usize,
}

impl Options {
    /// Fix a trusted principal and an explicit catalog/acquisition policy.
    /// `allow(service, None)` decides catalog visibility; `Some(key)` decides acquisition.
    /// Catalog visibility grants no key access. Method views still use the host's access policy.
    pub fn new(
        principal: String,
        allow: impl Fn(&str, Option<&[String]>) -> bool + 'static,
    ) -> Self {
        Self {
            principal,
            allow: Rc::new(allow),
            max_frame: 65_536,
            max_pending: 64,
            max_references: 32,
        }
    }
}

struct Claim {
    reference: Rc<Reference>,
    methods: BTreeSet<String>,
    observation: Option<Observation>,
    active: bool,
    pending: Option<Vec<u8>>,
}

struct Inner {
    host: Host,
    send: Sender,
    options: Options,
    closed: Cell<bool>,
    closed_waker: futures::task::AtomicWaker,
    cause: RefCell<Option<Error>>,
    next: Cell<u64>,
    claims: RefCell<BTreeMap<u64, Claim>>,
    requests: RefCell<BTreeMap<u64, bool>>,
    calls: RefCell<BTreeMap<u64, Context>>,
}

/// One byte channel's claims, calls and observations. Dropping it closes that channel.
/// This handle is local to the host's executor and is not `Send`.
pub struct Connection(Rc<Inner>);

/// Serve the existing SDK RPC vocabulary over an externally owned byte channel.
///
/// The sender must accept the entire frame synchronously or return a structured error. It must
/// never block the host executor: move blocking I/O elsewhere and refuse a full queue. It must
/// bound queued **and in-flight** bytes itself. This adapter retains at most one bounded State frame
/// per attaching claim, coalesced until its acquisition reply is accepted. A sender
/// error closes the logical connection, without retrying the frame. Returning `Ok(())` transfers
/// delivery responsibility to the embedding; it does not acknowledge peer receipt.
///
/// The embedding must monitor [`Connection::closed`] alongside input/output and close its actual
/// transport even when neither stream produces more traffic. Transport failure should call
/// [`Connection::close_with`]. Closing cancels calls and releases claims, but managed physical
/// callbacks retain their kernel pins until actual settlement. External effects are not undone.
///
/// ```
/// use futures::executor::LocalPool;
/// use tembra::{Host, rpc::{serve, Options}};
/// let mut executor = LocalPool::new();
/// let host = Host::new(executor.spawner());
/// let channel = serve(host.clone(), Options::new("reader".into(), |_, _| false), |_| Ok(()));
/// channel.close();
/// executor.run_until(channel.closed());
/// assert!(channel.is_closed());
/// assert!(channel.close_cause().is_none());
/// executor.run_until(host.close()).unwrap();
/// ```
pub fn serve(
    host: Host,
    options: Options,
    send: impl Fn(Vec<u8>) -> Result<()> + 'static,
) -> Connection {
    Connection(Rc::new(Inner {
        host,
        send: Rc::new(send),
        options,
        closed: Cell::new(false),
        closed_waker: futures::task::AtomicWaker::new(),
        cause: RefCell::new(None),
        next: Cell::new(0),
        claims: RefCell::new(BTreeMap::new()),
        requests: RefCell::new(BTreeMap::new()),
        calls: RefCell::new(BTreeMap::new()),
    }))
}

fn invalid(message: impl ToString) -> Error {
    Error::invalid("rpc_request", message.to_string())
}

fn size_error() -> Error {
    Error::new(Code::Exhausted, "frame_size", "encoded frame exceeds limit")
}

impl Connection {
    /// Admit one complete incoming JSON frame. Responses run on the host's executor.
    ///
    /// # Errors
    /// Malformed envelopes, duplicate active IDs or oversized input close this connection and
    /// return its cause. Payload errors and exhausted admission return structured request errors.
    /// No malformed frame affects another connection. Receiving after close returns `gone/connection`.
    pub fn receive(&self, bytes: &[u8]) -> Result<()> {
        if self.is_closed() {
            return Err(Error::new(Code::Gone, "connection", "connection is closed"));
        }
        let result = self.receive_open(bytes);
        if let Err(error) = &result {
            self.close_with(error.clone());
        }
        result
    }

    fn receive_open(&self, bytes: &[u8]) -> Result<()> {
        let inner = &self.0;
        if bytes.len() > inner.options.max_frame {
            return Err(size_error());
        }
        let request: Request = serde_json::from_slice(bytes).map_err(invalid)?;
        if let Op::Cancel = request.op {
            let id = request
                .payload
                .get("id")
                .and_then(Value::as_u64)
                .filter(|id| *id > 0 && *id <= MAX_ID)
                .ok_or_else(|| invalid("invalid cancellation id"))?;
            if let Some(cancelled) = inner.requests.borrow_mut().get_mut(&id) {
                *cancelled = true;
            }
            let context = inner.calls.borrow().get(&id).cloned();
            if let Some(context) = context {
                let _ = context.cancel();
            }
            return Ok(());
        }
        let id = request
            .id
            .filter(|id| *id > 0 && *id <= MAX_ID)
            .ok_or_else(|| invalid("invalid request id"))?;
        if inner.requests.borrow().contains_key(&id) {
            return Err(invalid("duplicate request id"));
        }
        if inner.requests.borrow().len() >= inner.options.max_pending {
            return inner.respond(
                id,
                Err(Error::new(
                    Code::Exhausted,
                    "pending_requests",
                    "pending request limit",
                )),
            );
        }
        inner.requests.borrow_mut().insert(id, false);
        let state = inner.clone();
        let spawned = inner.host.rpc_spawn(async move {
            let result = state.dispatch(id, request.op, request.payload).await;
            if !state.closed.get() {
                let activated = if matches!(request.op, Op::Get) {
                    result
                        .as_ref()
                        .ok()
                        .and_then(|v| v.get("reference"))
                        .and_then(Value::as_u64)
                } else {
                    None
                };
                if let Err(error) = state.respond(id, result) {
                    state.close(Some(error));
                }
                if let Some(reference) = activated {
                    let pending = {
                        let mut claims = state.claims.borrow_mut();
                        claims.get_mut(&reference).and_then(|claim| {
                            claim.active = true;
                            claim.pending.take()
                        })
                    };
                    if let Some(bytes) = pending
                        && !state.closed.get()
                        && let Err(error) = (state.send)(bytes)
                    {
                        state.close(Some(error));
                    }
                }
            }
            state.requests.borrow_mut().remove(&id);
            Ok(())
        });
        if spawned.is_err() {
            inner.requests.borrow_mut().remove(&id);
        }
        spawned
    }

    /// Cancel calls and release remote claims once. This does not wait for physical settlement.
    pub fn close(&self) {
        self.0.close(None);
    }

    /// Close with a terminal transport or policy cause. The first close and its cause win.
    pub fn close_with(&self, cause: Error) {
        self.0.close(Some(cause));
    }

    /// Wait for logical closure without requiring another incoming or outgoing frame.
    /// One embedding task may wait at a time; later polls replace the registered waker.
    /// Already closed connections complete immediately. This does not await callback settlement.
    pub async fn closed(&self) {
        futures::future::poll_fn(|cx| {
            self.0.closed_waker.register(cx.waker());
            if self.is_closed() {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await
    }

    /// Read the first terminal cause, or `None` for an open or explicitly closed connection.
    pub fn close_cause(&self) -> Option<Error> {
        self.0.cause.borrow().clone()
    }

    /// Read whether this connection has stopped admitting and emitting work.
    pub fn is_closed(&self) -> bool {
        self.0.closed.get()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

impl Inner {
    fn close(&self, cause: Option<Error>) {
        if self.closed.replace(true) {
            return;
        }
        *self.cause.borrow_mut() = cause;
        self.closed_waker.wake();
        let contexts: Vec<_> = self.calls.borrow().values().cloned().collect();
        for context in contexts {
            let _ = context.cancel();
        }
        let claims = std::mem::take(&mut *self.claims.borrow_mut());
        drop(claims);
    }

    fn encode(&self, message: &Value) -> Result<Vec<u8>> {
        struct Bounded {
            bytes: Vec<u8>,
            limit: usize,
        }
        impl Write for Bounded {
            fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
                if input.len() > self.limit.saturating_sub(self.bytes.len()) {
                    return Err(std::io::Error::other("frame limit"));
                }
                self.bytes.extend_from_slice(input);
                Ok(input.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut output = Bounded {
            bytes: Vec::new(),
            limit: self.options.max_frame,
        };
        serde_json::to_writer(&mut output, message).map_err(|_| size_error())?;
        Ok(output.bytes)
    }

    fn respond(&self, id: u64, result: Result<Value>) -> Result<()> {
        let message = match result {
            Ok(ok) => json!({"id":id,"ok":ok}),
            Err(error) => {
                json!({"id":id,"error":{"code":error.code.as_str(),"reason":error.reason,"message":error.message}})
            }
        };
        let bytes = self.encode(&message).or_else(|_| self.encode(&json!({"id":id,"error":{"code":"exhausted","reason":"frame_size","message":"encoded frame exceeds limit"}})))?;
        (self.send)(bytes)
    }

    fn state(&self, reference: u64, value: Value) {
        if self.closed.get() {
            return;
        }
        let bytes = match self
            .encode(&json!({"event":"state","payload":{"reference":reference,"value":value}}))
        {
            Ok(bytes) => bytes,
            Err(error) => {
                self.close(Some(error));
                return;
            }
        };
        let pending = {
            let mut claims = self.claims.borrow_mut();
            let Some(claim) = claims.get_mut(&reference) else {
                return;
            };
            if claim.active {
                Some(bytes)
            } else {
                claim.pending = Some(bytes);
                None
            }
        };
        if let Some(bytes) = pending
            && let Err(error) = (self.send)(bytes)
        {
            self.close(Some(error));
        }
    }

    async fn dispatch(self: &Rc<Self>, id: u64, op: Op, payload: Value) -> Result<Value> {
        if self.closed.get() || self.requests.borrow().get(&id) == Some(&true) {
            return Err(Error::new(Code::Cancelled, "request", "request cancelled"));
        }
        match op {
            Op::Describe => Ok(self.describe()),
            Op::Get => self.get(id, payload).await,
            Op::Release => {
                let reference = payload
                    .get("reference")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid("missing reference"))?;
                let claim = self.claims.borrow_mut().remove(&reference);
                drop(claim);
                Ok(Value::Null)
            }
            Op::Call => self.call(id, payload).await,
            Op::Unknown => Err(Error::invalid("rpc_operation", "unknown RPC operation")),
            Op::Cancel => unreachable!(),
        }
    }

    fn describe(&self) -> Value {
        Value::Array(self.host.kernel().installed_orbs().into_iter().filter_map(|orb| {
            let services: Vec<_> = orb.declaration.services.iter()
                .filter(|service| (self.options.allow)(&service.name, None))
                .map(|service| json!({"name":service.name,"methods":self.host.rpc_method_surface(&service.name, &self.options.principal)}))
                .collect();
            (!services.is_empty()).then(|| json!({"name":orb.declaration.name,"apiVersion":orb.declaration.api_version,"services":services}))
        }).collect())
    }

    async fn get(self: &Rc<Self>, id: u64, payload: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Get {
            service: String,
            key: Vec<String>,
        }
        let input: Get = serde_json::from_value(payload).map_err(invalid)?;
        if !(self.options.allow)(&input.service, Some(&input.key)) {
            return Err(Error::new(
                Code::Forbidden,
                "service",
                "service is not exposed",
            ));
        }
        self.check_references()?;
        let reference = Rc::new(self.host.get(
            &input.service,
            input.key,
            &self.options.principal,
        )?);
        reference.ready().await?;
        if self.closed.get() || self.requests.borrow().get(&id) == Some(&true) {
            return Err(Error::new(
                Code::Cancelled,
                "request",
                "acquisition cancelled",
            ));
        }
        self.check_references()?;
        let next = self
            .next
            .get()
            .checked_add(1)
            .filter(|id| *id <= MAX_ID)
            .ok_or_else(|| {
                Error::new(
                    Code::Exhausted,
                    "remote_references",
                    "reference ids exhausted",
                )
            })?;
        let methods = reference.rpc_methods()?;
        let readable = reference.rpc_state()?;
        let mut result = json!({"reference":next,"methods":methods});
        if let Some(readable) = &readable {
            result["state"] = (*readable.value()?).clone();
        }
        self.encode(&json!({"id":id,"ok":result}))?;
        self.next.set(next);
        self.claims.borrow_mut().insert(
            next,
            Claim {
                reference,
                methods: methods.into_iter().collect(),
                observation: None,
                active: false,
                pending: None,
            },
        );
        if let Some(readable) = readable {
            let state: Weak<Self> = Rc::downgrade(self);
            let initial = Cell::new(true);
            let observed = readable.subscribe(move |value| {
                if !initial.replace(false)
                    && let Some(state) = state.upgrade()
                {
                    state.state(next, (*value).clone());
                }
                Ok(())
            });
            match observed {
                Ok(observation) => {
                    if let Some(claim) = self.claims.borrow_mut().get_mut(&next) {
                        claim.observation = Some(observation);
                    }
                }
                Err(error) => {
                    let claim = self.claims.borrow_mut().remove(&next);
                    drop(claim);
                    return Err(error);
                }
            }
        }
        Ok(result)
    }

    fn check_references(&self) -> Result<()> {
        if self.claims.borrow().len() >= self.options.max_references {
            Err(Error::new(
                Code::Exhausted,
                "remote_references",
                "reference limit",
            ))
        } else {
            Ok(())
        }
    }

    async fn call(self: &Rc<Self>, id: u64, payload: Value) -> Result<Value> {
        #[derive(Deserialize)]
        struct Call {
            reference: u64,
            method: String,
            arguments: Vec<Value>,
        }
        let input: Call = serde_json::from_value(payload).map_err(invalid)?;
        let reference = {
            let claims = self.claims.borrow();
            let claim = claims
                .get(&input.reference)
                .ok_or_else(|| Error::new(Code::Forbidden, "method", "unknown reference"))?;
            if !claim.methods.contains(&input.method) {
                return Err(Error::new(
                    Code::Forbidden,
                    "method",
                    "method is not admitted",
                ));
            }
            claim.reference.clone()
        };
        let state = self.clone();
        self.host
            .root(&self.options.principal, move |ctx| {
                async move {
                    if state.closed.get() || state.requests.borrow().get(&id) == Some(&true) {
                        return Err(Error::new(Code::Cancelled, "request", "request cancelled"));
                    }
                    state.calls.borrow_mut().insert(id, ctx.clone());
                    let arguments = Value::Array(input.arguments);
                    let result = reference.rpc_call(&ctx, &input.method, arguments).await;
                    state.calls.borrow_mut().remove(&id);
                    result
                }
                .boxed_local()
            })
            .await
    }
}
