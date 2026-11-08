//! Node forwards only scoped callback packets; Wasmtime stays on its existing isolated worker.
use napi::{
    Status,
    bindgen_prelude::{Function, Uint8Array},
    threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode},
};
use napi_derive::napi;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::AtomicU32},
};
use tembra_executor_wasm::{
    Binding, Engine, Grants, InterpreterPort, Limits, PortReply, PortTicket, PreparedSource,
};

mod component;
mod events;

type EventCallback = ThreadsafeFunction<String, (), String, Status, false>;
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
    pub fn new(bytes: Uint8Array, configuration: String, log: LogCallback) -> napi::Result<Self> {
        let configuration: Configuration =
            serde_json::from_str(&configuration).map_err(js_error)?;
        let mut engine = Engine::new(&bytes, configuration.limits)
            .map_err(js_error)?
            .on_log(move |record| {
                let packet =
                    json!({"orb":record.orb,"level":record.level,"message":record.message})
                        .to_string();
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

    /// Materialize a fresh isolated revision from a closed source artifact and installation grants.
    #[napi]
    pub fn open(
        &self,
        name: String,
        api_version: u32,
        prepared: String,
        bindings: String,
        grants: String,
    ) -> napi::Result<NodeInterpreterPort> {
        let prepared: PreparedSource = serde_json::from_str(&prepared).map_err(js_error)?;
        let bindings: BTreeMap<String, Binding> =
            serde_json::from_str(&bindings).map_err(js_error)?;
        let grants: Grants = serde_json::from_str(&grants).map_err(js_error)?;
        let port = self
            .engine
            .open_port(&name, api_version, &prepared, &bindings, &grants)
            .map_err(js_error)?;
        Ok(NodeInterpreterPort {
            port: Some(port),
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
    ticket: RefCell<Option<PortTicket>>,
    replies: Arc<Mutex<BTreeMap<u32, PortReply>>>,
    bridge: RefCell<Option<std::thread::JoinHandle<()>>>,
    sequence: Arc<AtomicU32>,
}

#[napi]
impl NodeInterpreterPort {
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
        let packet = serde_json::from_str(&packet).map_err(js_error)?;
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

    /// Resolve only the original request; late replies to revoked observers are harmless.
    #[napi]
    pub fn reply(&self, id: u32, packet: String) -> napi::Result<bool> {
        let reply = self.replies.lock().map_err(js_error)?.remove(&id);
        if let Some(reply) = reply {
            reply.complete(decode(&packet));
            Ok(true)
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
    /// Reject calls once the embedding adapter has released this revision.
    fn port(&self) -> napi::Result<&InterpreterPort> {
        self.port
            .as_ref()
            .ok_or_else(|| js_error("interpreter port is closed"))
    }
}

/// Preserve typed native errors across the platform transport without serializing executable data.
pub(super) fn envelope(result: tembra::Result<Value>) -> Value {
    match result {
        Ok(value) => json!({"ok":value}),
        Err(error) => {
            json!({"error":{"code":error.code.as_str(),"reason":error.reason,"message":error.message}})
        }
    }
}
/// Decode one response supplied by the trusted platform scope adapter.
fn decode(packet: &str) -> tembra::Result<Value> {
    let value: Value = serde_json::from_str(packet)
        .map_err(|error| tembra::Error::invalid("platform_reply", error.to_string()))?;
    if let Some(value) = value.get("ok") {
        return Ok(value.clone());
    }
    Err(tembra::Error::invalid(
        "platform_reply",
        value["error"]
            .as_str()
            .unwrap_or("platform operation failed"),
    ))
}
/// Convert infrastructure diagnostics to the Node binding's ordinary error type.
pub(super) fn js_error(error: impl ToString) -> napi::Error {
    napi::Error::from_reason(error.to_string())
}
