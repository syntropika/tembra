use super::{NativeArtifact, Owner, Reply, error};
use crate::MobileError;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};
use tembra_executor_wasm::{
    Binding, Engine, Grants, InterpreterPort, PortEvent, PortEvents, PortReply, PortTicket,
    ingress::{
        IngressEvent, IngressField, IngressPort, IngressRequestChunk, IngressRequestHead,
        IngressResponseFrame,
    },
};

/// One HTTP field; its value is raw bytes (`ByteArray` / `Data`).
#[derive(uniffi::Record)]
pub struct MobileField {
    pub name: String,
    pub value: Vec<u8>,
}

/// The incoming request head the host passes to `begin_ingress`.
/// Supply scheme and authority together, or neither; an origin-form target can retain its
/// `host` field without inventing a scheme.
#[derive(uniffi::Record)]
pub struct MobileRequestHead {
    pub method: String,
    pub scheme: Option<String>,
    pub authority: Option<String>,
    pub path_with_query: Option<String>,
    pub fields: Vec<MobileField>,
}

/// The host's answer to one `RequestPull`.
#[derive(uniffi::Enum)]
pub enum MobileRequestChunk {
    Data {
        bytes: Vec<u8>,
    },
    /// The end of the request body; empty `trailers` for none.
    End {
        trailers: Vec<MobileField>,
    },
    Error {
        message: String,
    },
}

/// One event of an incoming HTTP exchange; JSON appears only in host-request and completion
/// packets, bodies and fields are bytes.
#[derive(uniffi::Enum)]
pub enum MobileIngressEvent {
    Host {
        id: u32,
        asynchronous: bool,
        packet: String,
    },
    /// The exchange's completion envelope (JSON); `finish` follows.
    Complete {
        result: String,
    },
    Head {
        status: u16,
        fields: Vec<MobileField>,
    },
    RequestPull,
    Data {
        bytes: Vec<u8>,
    },
    Trailers {
        fields: Vec<MobileField>,
    },
    End,
    Error {
        message: String,
    },
    /// The handler's WIT `error-code` in the standard binary codec; never with a `Head`.
    Refused {
        code: Vec<u8>,
    },
}

fn ingress_fields(fields: Vec<MobileField>) -> Vec<IngressField> {
    fields
        .into_iter()
        .map(|field| IngressField {
            name: field.name,
            value: field.value,
        })
        .collect()
}

fn mobile_fields(fields: Vec<IngressField>) -> Vec<MobileField> {
    fields
        .into_iter()
        .map(|field| MobileField {
            name: field.name,
            value: field.value,
        })
        .collect()
}

#[derive(serde::Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum Event {
    Host {
        id: u32,
        asynchronous: bool,
        packet: String,
    },
    Complete {
        result: Reply<String>,
    },
}

struct State {
    port: InterpreterPort,
    ticket: Option<PortTicket>,
}
struct Exchange {
    events: PortEvents,
    /// The carrier of an incoming HTTP exchange.
    ingress: Option<IngressPort>,
    closed: AtomicBool,
    replies: Mutex<BTreeMap<u32, PortReply>>,
    sequence: Arc<AtomicU32>,
}

#[derive(uniffi::Object)]
pub struct NativePort {
    owner: Mutex<Option<Owner<State>>>,
    logs: super::logs::Logs,
    exchange: Mutex<Option<Arc<Exchange>>>,
    sequence: Arc<AtomicU32>,
}

impl NativePort {
    /// Run `job` with the active incoming HTTP carrier.
    fn carrier<R>(
        &self,
        job: impl FnOnce(&IngressPort) -> Result<R, MobileError>,
    ) -> Result<R, MobileError> {
        let exchange = self.active()?;
        let carrier = exchange
            .ingress
            .as_ref()
            .ok_or_else(|| error("No incoming HTTP exchange is active"))?;
        job(carrier)
    }

    /// Keep the executor port and its local ownership ticket on one dedicated thread.
    pub(super) fn new(
        engine: Engine,
        artifact: Arc<NativeArtifact>,
        bindings: BTreeMap<String, Binding>,
        grants: Grants,
        cli: tembra_executor_wasm::Cli,
    ) -> Result<Arc<Self>, MobileError> {
        let logs = super::logs::Logs::default();
        let engine = logs.attach(engine);
        let owner = Owner::new(move || {
            Ok(State {
                port: engine
                    .open_port(&artifact.artifact, &bindings, &grants, &cli)
                    .map_err(error)?,
                ticket: None,
            })
        })?;
        Ok(Arc::new(Self {
            owner: Mutex::new(Some(owner)),
            logs,
            exchange: Mutex::new(None),
            sequence: Arc::default(),
        }))
    }

    /// Serialize local transport operations independently of asynchronous host callbacks.
    fn command<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut State) -> Result<R, MobileError> + Send + 'static,
    ) -> Result<R, MobileError> {
        self.owner
            .lock()
            .map_err(error)?
            .as_ref()
            .ok_or(MobileError::Closed)?
            .call(job)
    }

    /// Retain the current exchange without holding a platform lock across an await.
    fn active(&self) -> Result<Arc<Exchange>, MobileError> {
        self.exchange
            .lock()
            .map_err(error)?
            .clone()
            .ok_or_else(|| error("No interpreter exchange is active"))
    }
}

#[uniffi::export]
impl NativePort {
    /// Return executable metadata already validated by the prepared contract.
    pub fn description(&self) -> Result<String, MobileError> {
        self.command(|state| Ok(state.port.description().to_string()))
    }

    /// Drain the bounded diagnostic batch owned by this revision.
    pub fn take_logs(&self) -> String {
        self.logs.take()
    }

    /// Start a synchronous guest declaration or an admitted callback through the same event path.
    pub fn begin(&self, packet: String, declaration: bool) -> Result<(), MobileError> {
        let mut active = self.exchange.lock().map_err(error)?;
        if active.is_some() {
            return Err(error("An interpreter exchange is already active"));
        }
        let events = self.command(move |state| {
            let (ticket, events) = if declaration {
                state.port.begin_declaration(packet)
            } else {
                state.port.begin(packet)
            }
            .map_err(error)?;
            state.ticket = Some(ticket);
            Ok(events)
        })?;
        *active = Some(Arc::new(Exchange {
            events,
            ingress: None,
            closed: AtomicBool::new(false),
            replies: Mutex::default(),
            sequence: self.sequence.clone(),
        }));
        Ok(())
    }

    /// Receive from the exchange's single event loop. Dropping a pending receive leaves the
    /// next event available to a later receive.
    pub async fn receive(&self) -> Result<String, MobileError> {
        let exchange = self.active()?;
        super::deliver(async move {
            match exchange.events.receive_async().await.map_err(error)? {
                PortEvent::Host {
                    packet,
                    asynchronous,
                    reply,
                } => {
                    let id = exchange
                        .sequence
                        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                        .map_err(|_| error("Response identity space exhausted"))?
                        + 1;
                    let mut replies = exchange.replies.lock().map_err(error)?;
                    if exchange.closed.load(Ordering::Acquire) {
                        return Err(MobileError::Closed);
                    }
                    replies.insert(id, reply);
                    Ok(serde_json::to_string(&Event::Host {
                        id,
                        asynchronous,
                        packet,
                    })
                    .expect("serializable host event"))
                }
                PortEvent::Complete(value) => {
                    exchange.replies.lock().map_err(error)?.clear();
                    Ok(serde_json::to_string(&Event::Complete {
                        result: Reply::from(value),
                    })
                    .expect("serializable completion"))
                }
                PortEvent::Ingress(_) => Err(error(
                    "Incoming HTTP exchanges are received with receive_ingress",
                )),
            }
        })
        .await?
    }

    /// Start an incoming HTTP exchange of this revision's host-local handler.
    ///
    /// `packet` is the `{"kind":"ingress",…}` control packet; the head travels typed. Receive
    /// its events with `receive_ingress`; `Complete` is followed by `finish`.
    pub fn begin_ingress(
        &self,
        packet: String,
        head: MobileRequestHead,
    ) -> Result<(), MobileError> {
        let mut active = self.exchange.lock().map_err(error)?;
        if active.is_some() {
            return Err(error("An interpreter exchange is already active"));
        }
        let head = IngressRequestHead {
            method: head.method,
            scheme: head.scheme,
            authority: head.authority,
            path_with_query: head.path_with_query,
            fields: ingress_fields(head.fields),
        };
        let (events, carrier) = self.command(move |state| {
            let (ticket, events, carrier) =
                state.port.begin_ingress(packet, head).map_err(error)?;
            state.ticket = Some(ticket);
            Ok((events, carrier))
        })?;
        *active = Some(Arc::new(Exchange {
            events,
            ingress: Some(carrier),
            closed: AtomicBool::new(false),
            replies: Mutex::default(),
            sequence: self.sequence.clone(),
        }));
        Ok(())
    }

    /// Receive from the incoming exchange's single event loop. Dropping a pending receive
    /// leaves the next event available to a later receive.
    pub async fn receive_ingress(&self) -> Result<MobileIngressEvent, MobileError> {
        let exchange = self.active()?;
        super::deliver(async move {
            Ok(
                match exchange.events.receive_async().await.map_err(error)? {
                    PortEvent::Host {
                        packet,
                        asynchronous,
                        reply,
                    } => {
                        let id = exchange
                            .sequence
                            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                            .map_err(|_| error("Response identity space exhausted"))?
                            + 1;
                        let mut replies = exchange.replies.lock().map_err(error)?;
                        if exchange.closed.load(Ordering::Acquire) {
                            return Err(MobileError::Closed);
                        }
                        replies.insert(id, reply);
                        MobileIngressEvent::Host {
                            id,
                            asynchronous,
                            packet,
                        }
                    }
                    PortEvent::Complete(value) => {
                        exchange.replies.lock().map_err(error)?.clear();
                        MobileIngressEvent::Complete {
                            result: serde_json::to_string(&Reply::from(value))
                                .expect("serializable completion"),
                        }
                    }
                    PortEvent::Ingress(IngressEvent::Head(head)) => MobileIngressEvent::Head {
                        status: head.status,
                        fields: mobile_fields(head.fields),
                    },
                    PortEvent::Ingress(IngressEvent::RequestPull) => {
                        MobileIngressEvent::RequestPull
                    }
                    PortEvent::Ingress(IngressEvent::Refused(code)) => {
                        MobileIngressEvent::Refused { code }
                    }
                    PortEvent::Ingress(IngressEvent::Response(frame)) => match frame {
                        IngressResponseFrame::Data(bytes) => MobileIngressEvent::Data { bytes },
                        IngressResponseFrame::Trailers(fields) => MobileIngressEvent::Trailers {
                            fields: mobile_fields(fields),
                        },
                        IngressResponseFrame::End => MobileIngressEvent::End,
                        IngressResponseFrame::Error(message) => {
                            MobileIngressEvent::Error { message }
                        }
                    },
                },
            )
        })
        .await?
    }

    /// Answer the pending `RequestPull`; `false` when the answer is no longer wanted.
    pub fn supply_request(&self, chunk: MobileRequestChunk) -> Result<bool, MobileError> {
        let chunk = match chunk {
            MobileRequestChunk::Data { bytes } => IngressRequestChunk::Data(bytes),
            MobileRequestChunk::End { trailers } => IngressRequestChunk::End {
                trailers: ingress_fields(trailers),
            },
            MobileRequestChunk::Error { message } => IngressRequestChunk::Error(message),
        };
        self.carrier(|carrier| carrier.supply_request(chunk).map_err(error))
    }

    /// Ask for the next response frame; it arrives as one `Data`, `Trailers`, `End` or `Error`.
    pub fn pull_response(&self) -> Result<(), MobileError> {
        self.carrier(|carrier| carrier.pull_response().map_err(error))
    }

    /// The client went away: the response is dropped and the guest's request body closes.
    pub fn disconnect(&self) -> Result<(), MobileError> {
        self.carrier(|carrier| {
            carrier.disconnect();
            Ok(())
        })
    }

    /// Resolve only the exact host request issued by this active exchange.
    pub fn reply(&self, id: u32, packet: String) -> Result<bool, MobileError> {
        let exchange = self.active()?;
        let reply = exchange.replies.lock().map_err(error)?.remove(&id);
        if let Some(reply) = reply {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "lowercase")]
            enum Answer {
                Ok(Box<serde_json::value::RawValue>),
                Error(String),
            }
            let result = match serde_json::from_str::<Answer>(&packet) {
                Ok(Answer::Ok(value)) => Ok(value.get().to_owned()),
                Ok(Answer::Error(message)) => {
                    Err(tembra::Error::invalid("platform_reply", message))
                }
                Err(e) => Err(tembra::Error::invalid("platform_reply", e.to_string())),
            };
            Ok(reply.complete(result))
        } else {
            Ok(false)
        }
    }

    /// Finish ownership only after the receiver observed physical worker completion.
    pub fn finish(&self) -> Result<(), MobileError> {
        self.command(|state| {
            if let Some(ticket) = state.ticket.take() {
                ticket.finish().map_err(error)?;
            }
            Ok(())
        })?;
        self.exchange.lock().map_err(error)?.take();
        Ok(())
    }

    /// Cancel the active callback after its caller was cancelled.
    ///
    /// Pending standard WASI calls settle with an error and the exchange still finishes only
    /// after physical completion. Without an active exchange this does nothing.
    pub fn cancel(&self) -> Result<(), MobileError> {
        self.command(|state| {
            if let Some(ticket) = &state.ticket {
                ticket.cancel();
            }
            Ok(())
        })
    }

    /// Release guest closures after their owning environment has physically drained.
    pub fn release(&self, environment: u64) -> Result<(), MobileError> {
        if environment > tembra::core::MAX_ID {
            return Err(error("Invalid environment identity"));
        }
        let owner = self.owner.lock().map_err(error)?;
        match owner.as_ref() {
            Some(owner) => owner.call(move |state| {
                state.port.release(environment);
                Ok(())
            }),
            None => Ok(()),
        }
    }

    /// Revoke unanswered host requests before joining a revision during runtime invalidation.
    pub fn invalidate(&self) {
        if let Some(exchange) = self
            .exchange
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let mut replies = exchange.replies.lock().unwrap_or_else(|e| e.into_inner());
            exchange.closed.store(true, Ordering::Release);
            replies.clear();
            // An invalidated runtime is a disconnect for an incoming HTTP exchange.
            if let Some(carrier) = &exchange.ingress {
                carrier.disconnect();
            }
            exchange.events.close();
        }
        let owner = self.owner.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(owner);
    }

    /// Refuse explicit close while the host still owns an admitted exchange.
    pub fn shutdown(&self) -> Result<(), MobileError> {
        if self.exchange.lock().map_err(error)?.is_some() {
            return Err(error(
                "Wait for physical interpreter settlement before closing",
            ));
        }
        let owner = self.owner.lock().map_err(error)?.take();
        drop(owner);
        Ok(())
    }
}

impl Drop for NativePort {
    /// Drop pending replies before the thread-affine ticket so invalidation cannot strand its VM.
    fn drop(&mut self) {
        self.invalidate();
    }
}
