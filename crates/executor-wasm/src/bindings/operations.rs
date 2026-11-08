//! Route scope-bound operations without transferring host callbacks into the guest.
use super::*;
use crate::protocol::{self, AsyncRequest, Handle, Request, ScopeId, WAIT_LIMIT_MILLISECONDS};
use futures::FutureExt;

impl Environment {
    /// Resolve immediate operations against the current host-issued scope only.
    pub(super) fn sync(&self, scope: &Scope, packet: &str) -> Result<Value> {
        let request = Request::decode(packet).map_err(failure)?;
        if matches!(scope, Scope::Validation)
            && !matches!(request, Request::StateRead { .. } | Request::Console { .. })
        {
            return Err(failure(
                "reload validators may only read their retained state",
            ));
        }
        if !request.survives_cancellation() {
            match scope {
                Scope::Call(context) => context.check()?,
                Scope::Open(signal) => signal.check()?,
                Scope::Declaration(_) | Scope::Validation => {}
            }
        }
        match request {
            Request::Console { .. } => self.session.log(packet),
            Request::Address {} => {
                let declaration = Self::declaration(scope)?;
                let address = match self.service {
                    None => protocol::Address {
                        key: Vec::new(),
                        principal: "host".to_owned(),
                    },
                    Some(_) => {
                        let address = declaration.address()?;
                        protocol::Address {
                            key: address.key,
                            principal: address.principal,
                        }
                    }
                };
                encode(&address)
            }
            Request::Orb {} => {
                Self::declaration(scope)?;
                // The root Orb environment has no parent; the guest observes `undefined`.
                encode(&self.parent.as_ref().map(|parent| parent.environment))
            }
            Request::StateDeclare {
                initial,
                version,
                validate,
            } => {
                let declaration = Self::declaration(scope)?;
                let state = declaration.state(initial, version, |_| Ok(()))?;
                if let Some(callback) = validate {
                    let check_state = state.clone();
                    let environment = self.clone();
                    declaration.validate_on_reload(move || {
                        environment.session.blocking(
                            "declare",
                            serde_json::to_string(&GuestCommand::Validate {
                                callback,
                                value: check_state.value()?.as_ref(),
                            })
                            .map_err(failure)?,
                            |packet, _| environment.sync(&Scope::Validation, packet),
                        )?;
                        Ok(())
                    })?;
                }
                let id = self.allocate()?;
                self.handles.states.borrow_mut().insert(id, state);
                encode(&id)
            }
            Request::StateRead { scope: owner, id } => {
                let state = self.state(owner, id)?;
                let snapshot = state.snapshot()?;
                snapshot_value(
                    state.value()?.as_ref().clone(),
                    snapshot.revision,
                    snapshot.version,
                )
            }
            Request::StateUpdate {
                scope: owner,
                id,
                value,
            } => {
                let context = Self::context(scope)?;
                let state = self.state(owner, id)?;
                let value = state.update(context, |_| Ok(value))?;
                let snapshot = state.snapshot()?;
                snapshot_value(Value::clone(&value), snapshot.revision, snapshot.version)
            }
            Request::ReferenceDeclare { service, key } => {
                let binding = self
                    .imports
                    .get(&service)
                    .ok_or_else(|| failure("reference is not a selected WIT Orb import"))?;
                let reference = Self::declaration(scope)?.get(binding, key)?;
                let id = self.allocate()?;
                self.handles
                    .references
                    .borrow_mut()
                    .insert(id, (reference, service));
                encode(&id)
            }
            Request::ReferenceRelease { scope: owner, id } => {
                self.reference(owner, id)?.0.release()?;
                Ok(Value::Null)
            }
            Request::ReferenceStatus { scope: owner, id } => {
                encode(self.reference(owner, id)?.0.status()?.as_str())
            }
            Request::ReferenceState { scope: owner, id } => {
                let (snapshot, value) = self.reference(owner, id)?.0.state_snapshot()?;
                snapshot_value(value, snapshot.revision, snapshot.version)
            }
            Request::ResourceDeclare {
                name,
                version,
                open,
                close,
                dependencies,
            } => self.resource(scope, &name, version, open, close, &dependencies),
            Request::ResourceRead { scope: owner, id } => {
                let value = *self
                    .selected_handles(owner)?
                    .resources
                    .borrow()
                    .get(&id)
                    .ok_or_else(|| failure("unknown resource handle"))?
                    .value()?;
                encode(&value)
            }
            Request::ContextCheck {} => {
                Self::context(scope)?;
                Ok(Value::Null)
            }
            Request::ContextPrincipal {} => encode(&Self::context(scope)?.principal()?),
            Request::ContextCancel {} => {
                Self::context(scope)?.cancel()?;
                Ok(Value::Null)
            }
            // Liveness only: WASI enforces grants, this keeps I/O inside an admitted callback.
            Request::IoCheck {} => match scope {
                Scope::Call(_) | Scope::Open(_) => Ok(Value::Null),
                Scope::Declaration(_) | Scope::Validation => Err(failure(
                    "I/O must run in a resource opener or an admitted call",
                )),
            },
            Request::SignalCancelled {} => encode(&match scope {
                Scope::Call(context) => context.signal().is_cancelled(),
                Scope::Open(signal) => signal.is_cancelled(),
                Scope::Declaration(_) | Scope::Validation => false,
            }),
            Request::Unsupported => Err(failure("unsupported host operation")),
        }
    }

    /// Declare open and close callbacks without letting the guest acknowledge settlement itself.
    fn resource(
        &self,
        scope: &Scope,
        name: &str,
        version: u32,
        open: u32,
        close: u32,
        dependencies: &[Handle],
    ) -> Result<Value> {
        let declaration = Self::declaration(scope)?;
        let dependencies = dependencies
            .iter()
            .map(|dependency| {
                self.selected_handles(dependency.scope)?
                    .resources
                    .borrow()
                    .get(&dependency.id)
                    .map(Resource::key)
                    .ok_or_else(|| failure("unknown resource dependency"))
            })
            .collect::<Result<Vec<_>>>()?;
        let opening = self.clone();
        let closing = self.clone();
        let resource = declaration.resource_async(
            name,
            version,
            &dependencies,
            move |signal| {
                async move {
                    let value = opening
                        .execute(
                            Scope::Open(signal),
                            GuestCommand::Callback {
                                callback: open,
                                value: None,
                            },
                        )
                        .await?;
                    serde_json::from_value::<u32>(value).map_err(failure)
                }
                .boxed_local()
            },
            move |value, context| {
                async move {
                    closing
                        .execute(
                            Scope::Call(context),
                            GuestCommand::Callback {
                                callback: close,
                                value: Some(*value),
                            },
                        )
                        .await?;
                    Ok(())
                }
                .boxed_local()
            },
        )?;
        let id = self.allocate()?;
        self.handles.resources.borrow_mut().insert(id, resource);
        encode(&id)
    }

    /// Await portable references and cooperative timers under the active invocation's authority.
    pub(super) async fn asynchronous(&self, scope: &Scope, packet: &str) -> Result<Value> {
        match AsyncRequest::decode(packet).map_err(failure)? {
            AsyncRequest::ReferenceReady { scope: owner, id } => {
                let (reference, _) = self.reference(owner, id)?;
                reference.ready().await?;
                Ok(Value::Null)
            }
            AsyncRequest::ReferenceCall {
                scope: owner,
                id,
                method,
                args,
            } => {
                let context = Self::context(scope)?;
                let (reference, binding) = self.reference(owner, id)?;
                let method = self
                    .contract
                    .import_method(&binding, &method)
                    .map_err(failure)?;
                self.contract
                    .validate_import_arguments(&binding, &method, &args)
                    .map_err(failure)?;
                let value = reference.call_value(context, &method, args).await?;
                self.contract
                    .validate_import_result(&binding, &method, &value)
                    .map_err(failure)?;
                Ok(value)
            }
            AsyncRequest::Wait { milliseconds } => {
                if milliseconds > WAIT_LIMIT_MILLISECONDS {
                    return Err(failure("timer exceeds the callback wait limit"));
                }
                let signal = match scope {
                    Scope::Call(context) => {
                        context.check()?;
                        context.signal()
                    }
                    Scope::Open(signal) => signal.clone(),
                    Scope::Declaration(_) | Scope::Validation => {
                        return Err(failure("timer requires an active callback"));
                    }
                };
                signal.check()?;
                let wait = std::pin::pin!(futures_timer::Delay::new(
                    std::time::Duration::from_millis(milliseconds.into())
                ));
                let cancelled = std::pin::pin!(signal.cancelled());
                match futures::future::select(wait, cancelled).await {
                    futures::future::Either::Left(_) => Ok(Value::Null),
                    futures::future::Either::Right(_) => Err(failure("operation cancelled")),
                }
            }
            AsyncRequest::Unsupported => Err(failure("unsupported asynchronous host operation")),
        }
    }

    /// Resolve a state handle from this environment or its issued parent.
    fn state(&self, scope: Option<ScopeId>, id: u32) -> Result<State<Value>> {
        self.selected_handles(scope)?
            .states
            .borrow()
            .get(&id)
            .cloned()
            .ok_or_else(|| failure("unknown state handle"))
    }

    /// Resolve a reference handle and its WIT binding from this environment or its issued parent.
    fn reference(&self, scope: Option<ScopeId>, id: u32) -> Result<(Rc<Reference>, String)> {
        self.selected_handles(scope)?
            .references
            .borrow()
            .get(&id)
            .cloned()
            .ok_or_else(|| failure("unknown reference handle"))
    }
}

/// Encode a typed host reply for the guest transport.
fn encode<T: serde::Serialize + ?Sized>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(failure)
}

/// Return the kernel's snapshot revision rather than inventing guest state identities.
fn snapshot_value(value: Value, revision: u64, version: u32) -> Result<Value> {
    encode(&protocol::Snapshot {
        value,
        revision: revision.to_string(),
        version,
    })
}
