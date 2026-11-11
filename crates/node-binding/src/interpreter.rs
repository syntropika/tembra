//! Node forwards only scoped callback packets; Wasmtime stays on its existing isolated worker.
use napi::{
    Status,
    bindgen_prelude::{Buffer, Function, Uint8Array},
    threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode},
};
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::AtomicU32},
};
use tembra_executor_wasm::{
    Binding, Engine, Grants, InterpreterPort, Limits, PortReply, PortTicket,
    ingress::{IngressField, IngressPort, IngressRequestChunk, IngressRequestHead},
};

mod events;

type EventCallback = ThreadsafeFunction<String, (), String, Status, false>;
type IngressCallback = ThreadsafeFunction<NodeIngressEvent, (), NodeIngressEvent, Status, false>;

/// One HTTP field; its value is raw bytes.
#[napi(object)]
pub struct NodeField {
    pub name: String,
    pub value: Buffer,
}

/// The incoming request head the host passes to `ingress`.
#[napi(object)]
pub struct NodeIngressHead {
    pub method: String,
    pub scheme: Option<String>,
    pub authority: Option<String>,
    pub path_with_query: Option<String>,
    pub fields: Vec<NodeField>,
}

/// One event of an incoming HTTP exchange: an ordinary host request (`host`), its completion
/// (`complete`), or a typed carrier event (`head`, `request-pull`, `data`, `trailers`, `end`,
/// `error`, `refused`). Bytes stay `Buffer`s; JSON is used only for host request and completion
/// packets. `refused` carries the handler's WIT `error-code` in the standard binary codec.
#[napi(object)]
pub struct NodeIngressEvent {
    pub kind: String,
    pub id: Option<u32>,
    pub asynchronous: Option<bool>,
    pub packet: Option<String>,
    pub result: Option<String>,
    pub status: Option<u16>,
    pub fields: Option<Vec<NodeField>>,
    pub data: Option<Buffer>,
    pub message: Option<String>,
    /// `refused`: the `error-code` bytes.
    pub code: Option<Buffer>,
}

fn ingress_fields(fields: Vec<NodeField>) -> Vec<IngressField> {
    fields
        .into_iter()
        .map(|field| IngressField {
            name: field.name,
            value: field.value.to_vec(),
        })
        .collect()
}
type LogCallback = ThreadsafeFunction<String, (), String, Status, false, true>;

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct Configuration {
    limits: Limits,
    filesystem: BTreeMap<String, String>,
    network: Vec<String>,
}

/// A lazily constructed native interpreter backend; ordinary Node callbacks remain outside its VM.
#[napi]
pub struct NodeInterpreter {
    pub(super) engine: Engine,
}
#[napi]
impl NodeInterpreter {
    /// Compile the supplied reusable engine and register host-selected providers without any grants.
    #[napi(constructor)]
    pub fn new(configuration: String, log: LogCallback) -> napi::Result<Self> {
        let configuration: Configuration =
            serde_json::from_str(&configuration).map_err(js_error)?;
        let mut engine =
            Engine::new(configuration.limits)
                .map_err(js_error)?
                .on_log(move |record| {
                    let packet = serde_json::to_string(record).expect("serializable log record");
                    let _ = log.call(packet, ThreadsafeFunctionCallMode::NonBlocking);
                });
        for (name, path) in configuration.filesystem {
            engine = engine.filesystem(name, path).map_err(js_error)?;
        }
        for name in configuration.network {
            engine = engine.network(name).map_err(js_error)?;
        }
        Ok(Self { engine })
    }

    /// Configure interpreter code lazily; hosts that install only compiled components never load it.
    #[napi]
    pub fn interpreter(&mut self, bytes: Uint8Array) -> napi::Result<()> {
        self.engine = self.engine.clone().interpreter(&bytes).map_err(js_error)?;
        Ok(())
    }

    /// Materialize a fresh isolated revision from a closed source or compiled artifact.
    ///
    /// Both kinds return the same scoped port; compiled dependency calls arrive as ordinary host
    /// events on the dispatch bridge, never on libuv worker threads.
    #[napi]
    pub fn open(
        &self,
        artifact: &crate::NodeArtifact,
        bindings: String,
        grants: String,
        cli: Option<String>,
    ) -> napi::Result<NodeInterpreterPort> {
        let bindings: BTreeMap<String, Binding> =
            serde_json::from_str(&bindings).map_err(js_error)?;
        let grants: Grants = serde_json::from_str(&grants).map_err(js_error)?;
        // Data-only CLI configuration; byte streams are serviced in Rust, never per chunk here.
        let cli: tembra_executor_wasm::Cli = match cli {
            Some(cli) => serde_json::from_str(&cli).map_err(js_error)?,
            None => Default::default(),
        };
        let port = self
            .engine
            .open_port(&artifact.artifact, &bindings, &grants, &cli)
            .map_err(js_error)?;
        Ok(NodeInterpreterPort {
            port: Some(port),
            ingress: RefCell::new(None),
            ticket: RefCell::new(None),
            replies: Arc::default(),
            bridge: RefCell::new(None),
            sequence: Arc::default(),
        })
    }

    /// Count actual native Stores, including physical calls whose observers stopped waiting.
    #[napi]
    pub fn instances(&self) -> u32 {
        self.engine.instances().try_into().unwrap_or(u32::MAX)
    }
}

/// An endpoint is thread-affine; only its Send receiver and response senders cross the event bridge.
#[napi]
pub struct NodeInterpreterPort {
    port: Option<InterpreterPort>,
    /// The carrier of the active incoming HTTP exchange, if any.
    ingress: RefCell<Option<Arc<IngressPort>>>,
    ticket: RefCell<Option<PortTicket>>,
    replies: Arc<Mutex<BTreeMap<u32, PortReply>>>,
    bridge: RefCell<Option<std::thread::JoinHandle<()>>>,
    sequence: Arc<AtomicU32>,
}

#[napi]
impl NodeInterpreterPort {
    /// Cancel the active callback after its caller was cancelled.
    ///
    /// Pending standard WASI calls settle with an error and the exchange still finishes only
    /// after physical completion. Without an active exchange this does nothing.
    #[napi]
    pub fn cancel(&self) {
        if let Some(ticket) = self.ticket.borrow().as_ref() {
            ticket.cancel();
        }
    }

    /// Release guest closures without reentering a quarantined Store or inventing a lifecycle effect.
    #[napi]
    pub fn release(&self, environment: f64) -> napi::Result<()> {
        if !environment.is_finite()
            || environment.fract() != 0.0
            || environment < 0.0
            || environment > tembra::core::MAX_ID as f64
        {
            return Err(js_error("invalid environment identity"));
        }
        self.port()?.release(environment as u64);
        Ok(())
    }

    /// Read validated executable metadata without revealing VM objects or kernel handles.
    #[napi]
    pub fn description(&self) -> napi::Result<String> {
        Ok(self.port()?.description().to_string())
    }

    /// Execute a synchronous factory while the caller services only issued declaration operations.
    #[napi]
    pub fn declare(
        &self,
        packet: String,
        callback: Function<'_, String, String>,
    ) -> napi::Result<String> {
        if self.ticket.borrow().is_some() {
            return Err(js_error("interpreter exchange is already active"));
        }
        let packet = serde_json::from_str(&packet).map_err(js_error)?;
        let result = self.port()?.declare(packet, |packet, asynchronous| {
            if asynchronous {
                return Err(tembra::Error::invalid(
                    "factory",
                    "factories must be synchronous",
                ));
            }
            let answer = callback
                .call(packet.to_owned())
                .map_err(|error| tembra::Error::invalid("platform_callback", error.to_string()))?;
            decode(&answer)
        });
        Ok(envelope(result).to_string())
    }

    /// Dispatch callbacks without blocking Node's event loop or moving a local kernel to another thread.
    #[napi]
    pub fn dispatch(&self, packet: String, callback: EventCallback) -> napi::Result<()> {
        let port = self.port()?;
        if self.ticket.borrow().is_some() {
            return Err(js_error("interpreter exchange is already active"));
        }
        if let Some(thread) = self.bridge.borrow_mut().take() {
            let _ = thread.join();
        }
        let (start, receive) = std::sync::mpsc::channel::<tembra_executor_wasm::PortEvents>();
        let replies = self.replies.clone();
        let sequence = self.sequence.clone();
        let bridge = events::spawn(callback, receive, replies, sequence).map_err(js_error)?;
        let (ticket, events) = match port.begin(packet) {
            Ok(exchange) => exchange,
            Err(error) => {
                drop(start);
                let _ = bridge.join();
                return Err(js_error(error));
            }
        };
        *self.ticket.borrow_mut() = Some(ticket);
        if start.send(events).is_err() {
            self.ticket.borrow_mut().take();
            let _ = bridge.join();
            return Err(js_error("interpreter event bridge stopped before dispatch"));
        }
        *self.bridge.borrow_mut() = Some(bridge);
        Ok(())
    }

    /// Start an incoming HTTP exchange of this revision's host-local handler.
    ///
    /// `packet` is the `{"kind":"ingress",…}` control packet; the head travels typed. Events
    /// arrive on `callback` as typed objects, ordinary host requests included (answer them with
    /// `reply`); `complete` is followed by `finish`, exactly as for `dispatch`.
    #[napi]
    pub fn ingress(
        &self,
        packet: String,
        head: NodeIngressHead,
        callback: IngressCallback,
    ) -> napi::Result<()> {
        let port = self.port()?;
        if self.ticket.borrow().is_some() {
            return Err(js_error("interpreter exchange is already active"));
        }
        if let Some(thread) = self.bridge.borrow_mut().take() {
            let _ = thread.join();
        }
        let head = IngressRequestHead {
            method: head.method,
            scheme: head.scheme,
            authority: head.authority,
            path_with_query: head.path_with_query,
            fields: ingress_fields(head.fields),
        };
        let (ticket, events, carrier) = port.begin_ingress(packet, head).map_err(js_error)?;
        let carrier = Arc::new(carrier);
        let bridge = events::spawn_ingress(
            callback,
            events,
            carrier.clone(),
            self.replies.clone(),
            self.sequence.clone(),
        )
        .map_err(js_error)?;
        *self.ticket.borrow_mut() = Some(ticket);
        *self.ingress.borrow_mut() = Some(carrier);
        *self.bridge.borrow_mut() = Some(bridge);
        Ok(())
    }

    /// Answer the pending `request-pull`: a chunk, the end (`trailers`, `[]` for none) or an
    /// error; exactly one of the three. Returns `false` when the answer is no longer wanted.
    #[napi]
    pub fn supply_request(
        &self,
        data: Option<Uint8Array>,
        trailers: Option<Vec<NodeField>>,
        error: Option<String>,
    ) -> napi::Result<bool> {
        let chunk = match (data, trailers, error) {
            (Some(data), None, None) => IngressRequestChunk::Data(data.to_vec()),
            (None, Some(trailers), None) => IngressRequestChunk::End {
                trailers: ingress_fields(trailers),
            },
            (None, None, Some(message)) => IngressRequestChunk::Error(message),
            _ => return Err(js_error("supply exactly one of data, trailers or error")),
        };
        self.carrier()?.supply_request(chunk).map_err(js_error)
    }

    /// Ask for the next response frame; it arrives as one `data`, `trailers`, `end` or `error`
    /// event.
    #[napi]
    pub fn pull_response(&self) -> napi::Result<()> {
        self.carrier()?.pull_response().map_err(js_error)
    }

    /// The client went away: the response is dropped and the guest's request body closes.
    #[napi]
    pub fn disconnect(&self) -> napi::Result<()> {
        if let Some(carrier) = self.ingress.borrow().as_ref() {
            carrier.disconnect();
        }
        Ok(())
    }

    /// Resolve only the original request; late replies to revoked observers are harmless.
    #[napi]
    pub fn reply(&self, id: u32, packet: String) -> napi::Result<bool> {
        let reply = self.replies.lock().map_err(js_error)?.remove(&id);
        if let Some(reply) = reply {
            Ok(reply.complete(decode_raw(&packet)))
        } else {
            Ok(false)
        }
    }

    /// Complete the local owner ticket after the receiver observed actual worker settlement.
    #[napi]
    pub fn finish(&self) -> napi::Result<()> {
        if let Some(ticket) = self.ticket.borrow_mut().take() {
            ticket.finish().map_err(js_error)?;
        }
        // The exchange physically completed; its carrier owns nothing any more.
        self.ingress.borrow_mut().take();
        if let Some(thread) = self.bridge.borrow_mut().take() {
            let _ = thread.join();
        }
        Ok(())
    }

    /// Release an idle revision only after its host has cleaned up every owning scope.
    #[napi]
    pub fn close(&mut self) -> napi::Result<()> {
        if self.ticket.borrow().is_some() {
            return Err(js_error(
                "wait for physical interpreter settlement before closing",
            ));
        }
        self.finish()?;
        self.port.take();
        Ok(())
    }
}

impl NodeInterpreterPort {
    /// The active incoming HTTP carrier.
    fn carrier(&self) -> napi::Result<Arc<IngressPort>> {
        self.ingress
            .borrow()
            .clone()
            .ok_or_else(|| js_error("no incoming HTTP exchange is active"))
    }

    /// Reject calls once the embedding adapter has released this revision.
    fn port(&self) -> napi::Result<&InterpreterPort> {
        self.port
            .as_ref()
            .ok_or_else(|| js_error("interpreter port is closed"))
    }
}

/// Preserve typed native errors across the platform transport without serializing executable data.
pub(super) fn envelope<T: Serialize>(result: tembra::Result<T>) -> Value {
    let reply = match result {
        Ok(value) => Reply::Ok(value),
        Err(error) => Reply::Error(ReplyError {
            code: error.code.as_str(),
            reason: error.reason,
            message: error.message,
        }),
    };
    serde_json::to_value(reply).expect("serializable platform reply")
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Reply<T, E> {
    Ok(T),
    Error(E),
}
#[derive(Serialize)]
struct ReplyError {
    code: &'static str,
    reason: String,
    message: String,
}

/// Decode one response supplied by the trusted platform scope adapter.
fn decode(packet: &str) -> tembra::Result<Value> {
    let value: Reply<Value, String> = serde_json::from_str(packet)
        .map_err(|error| tembra::Error::invalid("platform_reply", error.to_string()))?;
    match value {
        Reply::Ok(value) => Ok(value),
        Reply::Error(message) => Err(tembra::Error::invalid("platform_reply", message)),
    }
}
/// Retain the checked payload verbatim; parsing through Value would rewrite its numbers.
fn decode_raw(packet: &str) -> tembra::Result<String> {
    let value: Reply<Box<serde_json::value::RawValue>, String> = serde_json::from_str(packet)
        .map_err(|error| tembra::Error::invalid("platform_reply", error.to_string()))?;
    match value {
        Reply::Ok(value) => Ok(value.get().to_owned()),
        Reply::Error(message) => Err(tembra::Error::invalid("platform_reply", message)),
    }
}

/// Convert infrastructure diagnostics to the Node binding's ordinary error type.
pub(super) fn js_error(error: impl ToString) -> napi::Error {
    napi::Error::from_reason(error.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_reply_preserves_checked_bytes() {
        let text = r#"{ "value": 1.0000000000000002, "zero": -0, "nested": [1e-30] }"#;
        assert_eq!(
            super::decode_raw(&format!(r#"{{"ok":{text}}}"#)).unwrap(),
            text
        );
        assert!(super::decode_raw(r#"{"ok": invalid}"#).is_err());
        assert!(super::decode_raw(r#"{"error":"refused"}"#).is_err());
    }
}
