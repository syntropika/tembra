//! Platform adapters route scoped host callbacks without embedding another lifecycle engine.
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

/// A thread-affine interpreter endpoint for an existing platform-owned Tembra kernel.
pub struct InterpreterPort {
    pub(crate) session: Session,
    pub(crate) description: Value,
    /// Checked once here so the native facade installs callbacks without revalidating the guest.
    pub(crate) services: Vec<tembra_contracts::ServiceBinding>,
    pub(crate) metadata: tembra::core::Orb,
    packet_bytes: usize,
}

impl Engine {
    /// Validate source, imports and grants before creating a platform-owned interpreter endpoint.
    pub fn open_port(
        &self,
        artifact: &tembra_contracts::Artifact,
        bindings: &BTreeMap<String, Binding>,
        grants: &Grants,
    ) -> Result<InterpreterPort> {
        let contract = &artifact.contract;
        let name = &artifact.identity.name;
        let api_version = artifact.identity.api_version;
        let tembra_contracts::ArtifactImplementation::Source(source) = &artifact.implementation
        else {
            return Err(failure("source interpreter requires a source artifact"));
        };
        let plan = contract
            .plan(name, api_version, bindings)
            .map_err(failure)?;
        contract.check_value_profile().map_err(failure)?;
        for (name, binding) in bindings {
            if matches!(binding, Binding::Orb { .. }) {
                contract.check_import_value_profile(name).map_err(failure)?;
            }
        }
        let io = crate::io::SourceIo::new(
            &self.providers,
            &plan.host_imports,
            grants,
            std::time::Duration::from_millis(self.limits.timeout_ms),
        )?;
        let session = Session::start(self.clone(), name.into(), io)?;
        let description = session.load(source, contract.describe())?;
        let services = contract
            .check_implementation(
                name,
                api_version,
                serde_json::from_value(description.clone()).map_err(failure)?,
            )
            .map_err(failure)?;
        Ok(InterpreterPort {
            session,
            description,
            services,
            metadata: plan.orb,
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
        let events = self.session.begin(Operation::Start, packet)?;
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
}
impl PortEvents {
    /// Forward an actual host request or physical completion to the embedding event loop.
    pub fn receive(&self) -> Result<PortEvent> {
        match self.events.recv_blocking().map_err(failure)? {
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
    /// Detect a revoked observer without treating cancellation as physical callback settlement.
    pub fn is_closed(&self) -> bool {
        self.reply.is_closed()
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
