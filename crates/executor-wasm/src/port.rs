//! Platform adapters route scoped host callbacks without embedding another lifecycle engine.
//! One port type serves source bundles and compiled components.
use crate::{
    Binding, Engine, Grants,
    abi::Operation,
    failure,
    worker::{self, Event, Session},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tembra::Result;

/// A thread-affine execution endpoint for an existing platform-owned Tembra kernel.
pub struct InterpreterPort {
    pub(crate) session: Session,
    pub(crate) description: Value,
    /// Checked once here so the native facade installs callbacks without revalidating the guest.
    pub(crate) services: Vec<tembra_contracts::ServiceBinding>,
    pub(crate) metadata: tembra::core::Orb,
    pub(crate) hooks: tembra_contracts::HookPlan,
    /// The checked host-local incoming HTTP handler, if this revision serves one.
    pub(crate) ingress: Option<tembra_contracts::IngressBinding>,
    /// Whether this revision exchanges capabilities through the native facade (see
    /// [`Transport`]): WIT resources of a compiled component, and whole byte-stream results.
    pub(crate) capabilities: bool,
    packet_bytes: usize,
}

/// What a revision may exchange with other Orbs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// JSON-compatible data only: platform ports and their foreign carriers.
    Data,
    /// Data plus host-issued capabilities, for the native Rust facade. Only compiled
    /// components serve WIT resources; a source Orb adds only `stream<u8>` returned as the whole
    /// result of an asynchronous freestanding export or import.
    Capabilities,
}

impl Engine {
    /// Validate an artifact, its selected imports and grants before creating its endpoint.
    ///
    /// Source bundles and compiled components return the same port: both execute behind one
    /// session owner and answer the same scoped host requests.
    pub fn open_port(
        &self,
        artifact: &tembra_contracts::Artifact,
        bindings: &BTreeMap<String, Binding>,
        grants: &Grants,
        cli: &crate::Cli,
    ) -> Result<InterpreterPort> {
        self.open(artifact, bindings, grants, cli, Transport::Data)
    }

    /// Open a port for the native facade or a platform host.
    ///
    /// Source and compiled handlers are served through the native facade and through platform
    /// ports' typed carrier (`begin_ingress`).
    pub(crate) fn open(
        &self,
        artifact: &tembra_contracts::Artifact,
        bindings: &BTreeMap<String, Binding>,
        grants: &Grants,
        cli: &crate::Cli,
        transport: Transport,
    ) -> Result<InterpreterPort> {
        let contract = &artifact.contract;
        let name = &artifact.identity.name;
        let api_version = artifact.identity.api_version;
        let mut plan = contract
            .plan(name, api_version, bindings)
            .map_err(failure)?;
        let capabilities = transport == Transport::Capabilities;
        let source = matches!(
            artifact.implementation,
            tembra_contracts::ArtifactImplementation::Source(_)
        );
        let imports = bindings
            .iter()
            .filter(|(_, binding)| matches!(binding, Binding::Orb { .. }))
            .map(|(name, _)| name);
        if capabilities && !source {
            contract.check_resource_profile().map_err(failure)?;
            for name in imports {
                contract
                    .check_import_resource_profile(name)
                    .map_err(failure)?;
            }
        } else {
            // A source export or import may return a whole byte stream only through the native
            // facade; data-only endpoints keep the data profiles.
            let (exports, import): (_, fn(&_, &str) -> _) = if capabilities {
                (
                    contract.check_source_profile(),
                    tembra_contracts::PreparedContract::check_import_source_profile,
                )
            } else {
                (
                    contract.check_value_profile(),
                    tembra_contracts::PreparedContract::check_import_value_profile,
                )
            };
            let data =
                exports.and_then(|()| imports.clone().try_for_each(|name| import(contract, name)));
            if let Err(error) = data {
                // Say so when only the capability transport is missing, not the shape itself.
                let streams = contract.check_source_profile().is_ok()
                    && imports
                        .clone()
                        .all(|name| contract.check_import_source_profile(name).is_ok());
                let resource_shapes = contract.check_resource_profile().is_ok()
                    && imports
                        .clone()
                        .all(|name| contract.check_import_resource_profile(name).is_ok());
                return Err(failure(if !capabilities && streams {
                    format!(
                        "{error}: byte-stream results are served only through the native Rust \
                         facade; this endpoint exchanges data only"
                    )
                } else if !source && resource_shapes {
                    format!(
                        "{error}: WIT resources are served only by compiled components installed \
                         through the native Rust facade; this endpoint exchanges data only"
                    )
                } else {
                    error.to_string()
                }));
            }
        }
        let checked = |description: &Value| {
            let implementation: tembra_contracts::Implementation =
                serde_json::from_value(description.clone()).map_err(failure)?;
            let services = contract
                .check_implementation(name, api_version, implementation.clone())
                .map_err(failure)?;
            let hooks = contract
                .check_hooks(&implementation, bindings)
                .map_err(failure)?;
            let ingress = contract
                .check_ingress(name, &implementation)
                .map_err(failure)?;
            Ok::<_, tembra::Error>((services, hooks, ingress))
        };
        let (session, description, (services, hooks, ingress)) = match &artifact.implementation {
            tembra_contracts::ArtifactImplementation::Source(source) => {
                let io =
                    crate::io::SourceIo::new(&self.providers, &plan.host_imports, grants, cli)?;
                let session = Session::start(self.clone(), name.into(), io)?;
                let description = session.load(source, contract)?;
                let checked = checked(&description)?;
                (session, description, checked)
            }
            tembra_contracts::ArtifactImplementation::Component(_) => {
                let description = crate::component::describe(
                    contract,
                    name,
                    api_version,
                    &artifact.contributions,
                    &artifact.hooks,
                    &artifact.services,
                );
                let checked = checked(&description)?;
                let session = self.component_session(
                    artifact,
                    &plan,
                    &checked.0,
                    bindings,
                    (grants, cli),
                    &checked.1,
                )?;
                (session, description, checked)
            }
        };
        hooks.apply(&mut plan);
        Ok(InterpreterPort {
            session,
            description,
            services,
            metadata: plan.orb,
            hooks,
            ingress,
            capabilities,
            packet_bytes: self.limits.packet_bytes,
        })
    }
}

impl InterpreterPort {
    /// Queue pure guest-map cleanup after the kernel releases the environment's physical owners.
    pub fn release(&self, environment: u64) {
        self.session.release_environment(environment);
    }

    /// Describe the already validated executable callbacks without publishing any kernel metadata.
    pub fn description(&self) -> &Value {
        &self.description
    }

    /// Enter one synchronous declaration pass under the embedding adapter's real scope.
    pub fn declare(
        &self,
        packet: Value,
        host: impl FnMut(&str, bool) -> Result<Value>,
    ) -> Result<Value> {
        self.session
            .blocking(Operation::Declare, packet.to_string(), host)
    }

    /// Split a platform exchange into a local owner ticket and a Send event receiver.
    pub fn begin(&self, packet: String) -> Result<(PortTicket, PortEvents)> {
        self.begin_operation(packet, Operation::Start)
    }

    /// Suspend synchronous guest factories at host requests without calling into a foreign JS stack.
    pub fn begin_declaration(&self, packet: String) -> Result<(PortTicket, PortEvents)> {
        self.begin_operation(packet, Operation::Declare)
    }

    /// Start an incoming HTTP exchange of this revision's host-local handler.
    ///
    /// `packet` is the control packet `{"kind":"ingress","environment","service","method"}`;
    /// the request travels typed beside it. The returned [`IngressPort`] answers request-body
    /// pulls and pulls the response; its events arrive on the same [`PortEvents`] stream as the
    /// exchange's host requests, always before its completion. See `crate::ingress` for the
    /// lifetime rules, which are the native facade's.
    ///
    /// [`IngressPort`]: crate::ingress::IngressPort
    pub fn begin_ingress(
        &self,
        packet: String,
        head: crate::ingress::IngressRequestHead,
    ) -> Result<(PortTicket, PortEvents, crate::ingress::IngressPort)> {
        if self.ingress.is_none() {
            return Err(failure("this revision serves no incoming HTTP handler"));
        }
        #[derive(serde::Deserialize)]
        #[serde(tag = "kind", rename_all = "lowercase")]
        enum Control {
            Ingress {},
        }
        serde_json::from_str::<Control>(&packet)
            .map_err(|_| failure("an incoming HTTP exchange needs an ingress control packet"))?;
        let (events, port) = self.session.begin_ingress(packet, head)?;
        let (ticket, events) = self.exchange(events);
        Ok((ticket, events, port))
    }

    /// Retain one owner until the selected guest exchange physically settles.
    fn begin_operation(
        &self,
        packet: String,
        operation: Operation,
    ) -> Result<(PortTicket, PortEvents)> {
        let events = self.session.begin(operation, packet)?;
        Ok(self.exchange(events))
    }

    /// Split one submitted exchange into its owner ticket and its event receiver.
    fn exchange(&self, events: async_channel::Receiver<Event>) -> (PortTicket, PortEvents) {
        let done = Arc::new(AtomicBool::new(false));
        (
            PortTicket {
                session: self.session.clone(),
                events: events.clone(),
                done: done.clone(),
                finished: false,
            },
            PortEvents {
                events,
                done,
                packet_bytes: self.packet_bytes,
                scope: self.session.scope(),
            },
        )
    }
}

/// The embedding thread retains this ticket until its receiver observes actual guest completion.
pub struct PortTicket {
    session: Session,
    events: async_channel::Receiver<Event>,
    done: Arc<AtomicBool>,
    finished: bool,
}
impl PortTicket {
    /// Cancel this exchange's callback after its caller was cancelled.
    ///
    /// Its epoch ends: pending standard WASI calls are revoked and settle with an error, so the
    /// callback can observe the cancellation; the Store is not quarantined. After the exchange
    /// has physically completed this does nothing, so a stale ticket never reaches a later
    /// callback.
    pub fn cancel(&self) {
        if !self.finished && !self.done.load(Ordering::Acquire) {
            self.session.scope().end_callback();
        }
    }

    /// Admit the next exchange only after the worker's completion event was received.
    pub fn finish(mut self) -> Result<()> {
        if !self.done.load(Ordering::Acquire) {
            return Err(failure("platform exchange has not physically completed"));
        }
        self.finished = true;
        self.session.finish_exchange();
        Ok(())
    }
}
impl Drop for PortTicket {
    /// Never let a dropped driver reuse a Store that could still be executing.
    fn drop(&mut self) {
        if !self.finished {
            self.session.abandon_exchange(&self.events);
        }
    }
}

/// This receiver contains no thread-affine kernel, callback or interpreter owner.
pub struct PortEvents {
    events: async_channel::Receiver<Event>,
    done: Arc<AtomicBool>,
    packet_bytes: usize,
    /// The callback's epoch ends here, before the embedding awaits pending host requests.
    scope: crate::io::CallbackScope,
}
impl PortEvents {
    /// Revoke pending requests when the embedding JavaScript runtime is invalidated.
    pub fn close(&self) {
        self.scope.end_callback();
        self.events.close();
        worker::close_replies(&self.events);
    }

    /// Forward an actual host request or physical completion to the embedding event loop.
    pub fn receive(&self) -> Result<PortEvent> {
        loop {
            if let Some(event) = self.event(self.events.recv_blocking().map_err(failure)?) {
                return Ok(event);
            }
        }
    }

    /// Await the next event without blocking an embedding runtime thread.
    pub async fn receive_async(&self) -> Result<PortEvent> {
        loop {
            if let Some(event) = self.event(self.events.recv().await.map_err(failure)?) {
                return Ok(event);
            }
        }
    }

    /// Mark physical completion before allowing the embedding owner to finish.
    fn event(&self, event: Event) -> Option<PortEvent> {
        match event {
            Event::Host {
                packet,
                asynchronous,
                reply,
            } => Some(PortEvent::Host {
                packet,
                asynchronous,
                reply: PortReply {
                    reply,
                    limit: self.packet_bytes,
                },
            }),
            Event::Done(result) => {
                // End this callback's epoch before the embedding drains its pending requests.
                self.scope.end_callback();
                self.done.store(true, Ordering::Release);
                let result = result.map_err(failure);
                Some(PortEvent::Complete(result))
            }
            Event::Ingress(event) => Some(PortEvent::Ingress(event)),
            // A forged private stream request is a per-request refusal. Keep driving this
            // exchange until Done; only that event proves its callback physically completed.
            Event::Stream { reply, .. } => {
                let refusal = "byte streams are served only through the native facade";
                let _ = reply.send(Err(refusal.into()));
                None
            }
        }
    }
}

impl Drop for PortEvents {
    /// Release buffered response senders if the embedding event loop stops consuming requests.
    fn drop(&mut self) {
        if !self.done.load(Ordering::Acquire) {
            self.scope.end_callback();
            worker::close_replies(&self.events);
        }
    }
}

/// A response can settle only its original request, including after the guest stops observing it.
pub struct PortReply {
    reply: tokio::sync::oneshot::Sender<std::result::Result<String, String>>,
    limit: usize,
}
impl PortReply {
    /// Publish to the original observer. A rejected delivery leaves ownership with the host.
    pub fn complete(self, result: Result<String>) -> bool {
        self.reply.send(worker::encode(result, self.limit)).is_ok()
    }
}

/// Platform code receives authority-bound requests, never a raw Tembra kernel capability.
pub enum PortEvent {
    Host {
        packet: String,
        asynchronous: bool,
        reply: PortReply,
    },
    Complete(Result<String>),
    /// Typed incoming HTTP carrier events of an exchange started with
    /// [`InterpreterPort::begin_ingress`].
    Ingress(crate::ingress::IngressEvent),
}
