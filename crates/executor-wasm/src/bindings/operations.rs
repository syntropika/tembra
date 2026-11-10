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
            Self::authorize(scope)?;
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
                            crate::abi::Operation::Declare,
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
            Request::StateRead {
                scope: owner,
                id,
                known,
            } => {
                // Handle ownership and the live kernel snapshot are checked on every read.
                let state = self.state(owner, id)?;
                let snapshot = state.snapshot()?;
                snapshot_reply(
                    snapshot.revision,
                    snapshot.version,
                    known.as_deref(),
                    || Ok(state.value()?.as_ref().clone()),
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
            Request::HookDeclare { name } => {
                let declaration = Self::declaration(scope)?;
                let binding = self
                    .hook_bindings
                    .get(&name)
                    .ok_or_else(|| failure("hook is absent from the checked WIT declaration"))?;
                use tembra_contracts::HookComposition;
                let handle = match binding.composition {
                    HookComposition::Series => BoundHook::Series(
                        declaration.hook(
                            &tembra::Hook::<Value, ()>::series(&binding.name)
                                .signature(&binding.signature),
                        )?,
                        binding.slot,
                    ),
                    HookComposition::Waterfall => BoundHook::Waterfall(
                        declaration.hook(
                            &tembra::Hook::<Value, Value>::waterfall(&binding.name)
                                .signature(&binding.signature),
                        )?,
                        binding.slot,
                    ),
                    HookComposition::Bail => BoundHook::Bail(
                        declaration.hook(
                            &tembra::Hook::<Value, Option<Value>>::bail(&binding.name)
                                .signature(&binding.signature),
                        )?,
                        binding.slot,
                    ),
                };
                let id = self.allocate()?;
                self.handles.hooks.borrow_mut().insert(id, handle);
                encode(&id)
            }
            Request::ReferenceDeclare { service, key } => {
                let binding = self
                    .imports
                    .get(&service)
                    .ok_or_else(|| failure("reference is not a selected WIT Orb import"))?;
                let reference = Self::declaration(scope)?.get(&binding.service, key)?;
                let id = self.allocate()?;
                self.handles.references.borrow_mut().insert(
                    id,
                    ReferenceHandle {
                        reference,
                        import: binding.clone(),
                    },
                );
                encode(&id)
            }
            Request::ReferenceRelease { scope: owner, id } => {
                self.reference(owner, id)?.reference.release()?;
                Ok(Value::Null)
            }
            Request::ReferenceStatus { scope: owner, id } => {
                encode(self.reference(owner, id)?.reference.status()?.as_str())
            }
            Request::ReferenceState {
                scope: owner,
                id,
                known,
            } => {
                let (snapshot, value) = self.reference(owner, id)?.reference.state_snapshot()?;
                snapshot_reply(
                    snapshot.revision,
                    snapshot.version,
                    known.as_deref(),
                    || Ok(value),
                )
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
                    let cancellation = signal.clone();
                    let value = opening
                        .execute(
                            Scope::Open(signal),
                            Some(cancellation),
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
                    // Cleanup must reach the guest even when its host is shutting down.
                    closing
                        .execute(
                            Scope::Call(context),
                            None,
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
        let request = AsyncRequest::decode(packet).map_err(failure)?;
        if request.is_io() {
            // Closing an owned stream is cleanup; it needs a callback but survives cancellation.
            let cleanup = request.survives_cancellation();
            let signal = match scope {
                Scope::Call(context) => {
                    if !cleanup {
                        context.check()?;
                    }
                    context.signal()
                }
                Scope::Open(signal) => {
                    if !cleanup {
                        signal.check()?;
                    }
                    signal.clone()
                }
                Scope::Declaration(_) | Scope::Validation => {
                    return Err(failure("I/O requires an active callback"));
                }
            };
            let operation = self.session.io_operation(packet)?.execute();
            if !request.is_transport() {
                return operation.await;
            }
            // Cancellation ends this callback's transport authority: its streams close, so the
            // pending socket work settles now; it is still awaited to physical completion.
            let mut operation = std::pin::pin!(operation);
            let cancelled = std::pin::pin!(signal.cancelled());
            return match futures::future::select(operation.as_mut(), cancelled).await {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => {
                    self.session.streams().end_callback();
                    operation.await
                }
            };
        }
        match request {
            AsyncRequest::HookRun {
                scope: owner,
                id,
                value,
            } => {
                let context = Self::context(scope)?;
                let handles = self.selected_handles(owner)?;
                let hook = handles
                    .hooks
                    .borrow()
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| failure("unknown hook handle"))?;
                self.contract
                    .validate_slot(hook.slot(), true, &Value::Array(vec![value.clone()]))
                    .map_err(failure)?;
                let value = hook.run(context, value).await?;
                self.contract
                    .validate_slot(hook.slot(), false, &value)
                    .map_err(failure)?;
                Ok(value)
            }
            AsyncRequest::ReferenceReady { scope: owner, id } => {
                let signal = Self::signal(scope, "readiness requires an active callback")?;
                let reference = self.reference(owner, id)?.reference;
                // Only the logical wait ends; activation stays owned by the kernel.
                let ready = std::pin::pin!(reference.ready());
                let cancelled = std::pin::pin!(signal.cancelled());
                match futures::future::select(ready, cancelled).await {
                    futures::future::Either::Left((result, _)) => result.map(|()| Value::Null),
                    futures::future::Either::Right(_) => Err(failure("operation cancelled")),
                }
            }
            AsyncRequest::ReferenceCall {
                scope: owner,
                id,
                method,
                args,
            } => {
                let context = Self::context(scope)?;
                let ReferenceHandle {
                    reference,
                    import: binding,
                } = self.reference(owner, id)?;
                let method = binding
                    .methods
                    .get(&method)
                    .ok_or_else(|| failure("unknown prepared import method"))?;
                self.contract
                    .validate_slot(method.slot, true, &args)
                    .map_err(failure)?;
                let value = reference.call_value(context, &method.name, args).await?;
                self.contract
                    .validate_slot(method.slot, false, &value)
                    .map_err(failure)?;
                Ok(value)
            }
            AsyncRequest::Wait { milliseconds } => {
                if milliseconds > WAIT_LIMIT_MILLISECONDS {
                    return Err(failure("timer exceeds the callback wait limit"));
                }
                let signal = Self::signal(scope, "timer requires an active callback")?;
                let wait = std::pin::pin!(futures_timer::Delay::new(
                    std::time::Duration::from_millis(milliseconds.into())
                ));
                let cancelled = std::pin::pin!(signal.cancelled());
                match futures::future::select(wait, cancelled).await {
                    futures::future::Either::Left(_) => Ok(Value::Null),
                    futures::future::Either::Right(_) => Err(failure("operation cancelled")),
                }
            }
            AsyncRequest::TransportConnect { .. }
            | AsyncRequest::TransportRead { .. }
            | AsyncRequest::TransportWrite { .. }
            | AsyncRequest::TransportClose { .. } => {
                unreachable!("I/O was handled under callback authority")
            }
            AsyncRequest::Unsupported => Err(failure("unsupported asynchronous host operation")),
        }
    }

    /// Select the live cancellation that owns a cooperative wait in this callback.
    fn signal(scope: &Scope, inactive: &'static str) -> Result<Cancellation> {
        let signal = match scope {
            Scope::Call(context) => {
                context.check()?;
                context.signal()
            }
            Scope::Open(signal) => signal.clone(),
            Scope::Declaration(_) | Scope::Validation => return Err(failure(inactive)),
        };
        signal.check()?;
        Ok(signal)
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
    fn reference(&self, scope: Option<ScopeId>, id: u32) -> Result<ReferenceHandle> {
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

/// Answer a snapshot read. When the guest names the current revision it already retains, only
/// the identity is returned: one kernel revision never names two values, so the protected guest
/// copy stays authoritative for that revision and the value is neither cloned nor serialized.
fn snapshot_reply(
    revision: u64,
    version: u32,
    known: Option<&str>,
    value: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    let current = revision.to_string();
    if known == Some(current.as_str()) {
        let mut reply = serde_json::Map::with_capacity(2);
        reply.insert("revision".into(), Value::String(current));
        reply.insert("version".into(), Value::from(version));
        return Ok(Value::Object(reply));
    }
    snapshot_value(value()?, revision, version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_snapshot_reads_omit_only_the_value() {
        let full = snapshot_reply(7, 2, None, || Ok(serde_json::json!({"a": [1]}))).unwrap();
        assert_eq!(
            full,
            serde_json::json!({"value": {"a": [1]}, "revision": "7", "version": 2})
        );
        let stale = snapshot_reply(7, 2, Some("6"), || Ok(serde_json::json!(3))).unwrap();
        assert_eq!(
            stale,
            serde_json::json!({"value": 3, "revision": "7", "version": 2})
        );
        let unchanged = snapshot_reply(7, 2, Some("7"), || -> Result<Value> {
            panic!("an unchanged revision must not read its value")
        })
        .unwrap();
        assert_eq!(
            unchanged,
            serde_json::json!({"revision": "7", "version": 2})
        );
    }
}
