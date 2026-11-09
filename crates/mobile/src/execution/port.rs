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
};

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
    /// Keep the executor port and its local ownership ticket on one dedicated thread.
    pub(super) fn new(
        engine: Engine,
        artifact: Arc<NativeArtifact>,
        bindings: BTreeMap<String, Binding>,
        grants: Grants,
    ) -> Result<Arc<Self>, MobileError> {
        let logs = super::logs::Logs::default();
        let engine = logs.attach(engine);
        let owner = Owner::new(move || {
            Ok(State {
                port: engine
                    .open_port(&artifact.artifact, &bindings, &grants)
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
            closed: AtomicBool::new(false),
            replies: Mutex::default(),
            sequence: self.sequence.clone(),
        }));
        Ok(())
    }

    /// Suspend the foreign promise until an actual host request or physical completion arrives.
    pub async fn receive(&self) -> Result<String, MobileError> {
        let exchange = self.active()?;
        super::delivery()?
            .spawn(async move {
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
                }
            })
            .await
            .map_err(error)?
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
            reply.complete(result);
            Ok(true)
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

    /// Execute admitted filesystem and socket operations using the original installation grants.
    pub async fn io(&self, packet: String) -> Result<String, MobileError> {
        self.active()?;
        let operation =
            self.command(move |state| state.port.io_operation(&packet).map_err(error))?;
        let (send, receive) = futures::channel::oneshot::channel();
        operation.submit(move |result| {
            let _ = send.send(result.map(|value| value.to_string()).map_err(error));
        });
        receive.await.map_err(error)?
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
