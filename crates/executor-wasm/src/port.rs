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
    packet_bytes: usize,
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
    ) -> Result<InterpreterPort> {
        let contract = &artifact.contract;
        let name = &artifact.identity.name;
        let api_version = artifact.identity.api_version;
        let mut plan = contract
            .plan(name, api_version, bindings)
            .map_err(failure)?;
        contract.check_value_profile().map_err(failure)?;
        for (name, binding) in bindings {
            if matches!(binding, Binding::Orb { .. }) {
                contract.check_import_value_profile(name).map_err(failure)?;
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
            Ok::<_, tembra::Error>((services, hooks))
        };
        let (session, description, services, hooks) = match &artifact.implementation {
            tembra_contracts::ArtifactImplementation::Source(source) => {
                let io = crate::io::SourceIo::new(
                    &self.providers,
                    &plan.host_imports,
                    grants,
                    std::time::Duration::from_millis(self.limits.timeout_ms),
                )?;
                let session = Session::start(self.clone(), name.into(), io)?;
                let description = session.load(source, contract.describe())?;
                let (services, hooks) = checked(&description)?;
                (session, description, services, hooks)
            }
            tembra_contracts::ArtifactImplementation::Component(_) => {
                let description = crate::component::describe(
                    contract,
                    name,
                    api_version,
                    &artifact.contributions,
                );
                let (services, hooks) = checked(&description)?;
                let session =
                    self.component_session(artifact, &plan, &services, bindings, grants, &hooks)?;
                (session, description, services, hooks)
            }
        };
        hooks.apply(&mut plan);
        Ok(InterpreterPort {
            session,
            description,
            services,
            metadata: plan.orb,
            hooks,
            packet_bytes: self.limits.packet_bytes,
        })
    }
}

impl InterpreterPort {
    /// Prepare host I/O after the embedding scope has checked callback authority.
    pub fn io_operation(&self, packet: &str) -> Result<crate::IoOperation> {
        self.session.io_operation(packet)
    }

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

    /// Retain one owner until the selected guest exchange physically settles.
    fn begin_operation(
        &self,
        packet: String,
        operation: Operation,
    ) -> Result<(PortTicket, PortEvents)> {
        let events = self.session.begin(operation, packet)?;
        let done = Arc::new(AtomicBool::new(false));
        Ok((
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
                streams: self.session.streams(),
            },
        ))
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
    /// The callback's transport epoch ends here, before the embedding awaits pending I/O.
    streams: crate::io::Streams,
}
impl PortEvents {
    /// Revoke pending requests when the embedding JavaScript runtime is invalidated.
    pub fn close(&self) {
        self.streams.end_callback();
        self.events.close();
        worker::close_replies(&self.events);
    }

    /// Forward an actual host request or physical completion to the embedding event loop.
    pub fn receive(&self) -> Result<PortEvent> {
        self.event(self.events.recv_blocking().map_err(failure)?)
    }

    /// Await the next event without blocking an embedding runtime thread.
    pub async fn receive_async(&self) -> Result<PortEvent> {
        self.event(self.events.recv().await.map_err(failure)?)
    }

    /// Mark physical completion before allowing the embedding owner to finish.
    fn event(&self, event: Event) -> Result<PortEvent> {
        match event {
            Event::Host {
                packet,
                asynchronous,
                reply,
            } => Ok(PortEvent::Host {
                packet,
                asynchronous,
                reply: PortReply {
                    reply,
                    limit: self.packet_bytes,
                },
            }),
            Event::Done(result) => {
                // Close this callback's streams before the embedding drains its pending I/O.
                self.streams.end_callback();
                self.done.store(true, Ordering::Release);
                let result = result.map_err(failure);
                Ok(PortEvent::Complete(result))
            }
        }
    }
}

impl Drop for PortEvents {
    /// Release buffered response senders if the embedding event loop stops consuming requests.
    fn drop(&mut self) {
        if !self.done.load(Ordering::Acquire) {
            self.streams.end_callback();
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
    /// Resolve one host request without exposing native kernel handles or executable callbacks.
    pub fn complete(self, result: Result<String>) {
        let _ = self.reply.send(worker::encode(result, self.limit));
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
}
