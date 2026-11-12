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
                let id = self.handles.allocate()?;
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
                let id = self.handles.allocate()?;
                self.handles.hooks.borrow_mut().insert(id, handle);
                encode(&id)
            }
            Request::ReferenceDeclare { service, key } => {
                let binding = self
                    .imports
                    .get(&service)
                    .ok_or_else(|| failure("reference is not a selected WIT Orb import"))?;
                let reference = Self::declaration(scope)?.get(&binding.service, key)?;
                let id = self.handles.allocate()?;
                self.handles.references.borrow_mut().insert(
                    id,
                    ReferenceHandle {
                        claim: Claim::Declared(reference),
                        import: binding.clone(),
                    },
                );
                encode(&id)
            }
            Request::ReferenceRelease { scope: owner, id } => {
                // A compiled guest drops imported handles and views in the Orb table. Its
                // never-reused ids cannot accidentally name a factory-declared reference.
                if !self.release_import(owner, id)? {
                    self.reference(scope, owner, id)?.0.release()?;
                }
                Ok(Value::Null)
            }
            Request::ReferenceStatus { scope: owner, id } => {
                encode(self.reference(scope, owner, id)?.0.status()?.as_str())
            }
            Request::ReferenceState {
                scope: owner,
                id,
                known,
            } => {
                let (snapshot, value) = self.reference(scope, owner, id)?.0.state_snapshot()?;
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
                            Carrier::None,
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
                            Carrier::None,
                        )
                        .await?;
                    Ok(())
                }
                .boxed_local()
            },
        )?;
        let id = self.handles.allocate()?;
        self.handles.resources.borrow_mut().insert(id, resource);
        encode(&id)
    }

    /// Await portable references and cooperative timers under the active invocation's authority.
    /// `issued` receives the dependency call an import call issues, so the request's answer can
    /// cancel it and wait for its physical end if the callback stops waiting. A request on the
    /// stream carrier (`stream`) is only ever an import call.
    pub(super) async fn asynchronous(
        &self,
        scope: &Scope,
        packet: &str,
        stream: bool,
        moves: Option<crate::worker::arguments::Moves>,
        issued: &crate::worker::Issued,
    ) -> Result<crate::worker::Reply> {
        let request = AsyncRequest::decode(packet).map_err(failure)?;
        // An import call may deliver owned resources, committed only once the worker has them.
        if let AsyncRequest::ReferenceCall {
            scope: owner,
            id,
            method,
            args,
        } = request
        {
            return self
                .call_import(scope, owner, id, &method, args, stream, moves, issued)
                .await;
        }
        if stream || moves.is_some() {
            return Err(failure("a capability carrier carries only an import call"));
        }
        if let AsyncRequest::ReferenceAcquire { service, key } = request {
            return self.acquire(scope, &service, key).await;
        }
        let value = match request {
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
                // The guest's packet is wire form; the kernel and other Orbs see exact numbers.
                let value = match self
                    .contract
                    .from_wire(hook.slot(), true, Value::Array(vec![value]))
                    .map_err(failure)?
                {
                    Value::Array(mut values) if values.len() == 1 => values.remove(0),
                    _ => return Err(failure("a checked hook argument changed shape")),
                };
                let value = hook.run(context, value).await?;
                self.contract
                    .to_wire(hook.slot(), false, value)
                    .map_err(failure)
            }
            AsyncRequest::ReferenceReady { scope: owner, id } => {
                let signal = Self::signal(scope, "readiness requires an active callback")?;
                let (reference, _) = self.reference(scope, owner, id)?;
                // Only the logical wait ends; activation stays owned by the kernel.
                let ready = std::pin::pin!(reference.ready());
                let cancelled = std::pin::pin!(signal.cancelled());
                match futures::future::select(ready, cancelled).await {
                    futures::future::Either::Left((result, _)) => result.map(|()| Value::Null),
                    futures::future::Either::Right(_) => Err(failure("operation cancelled")),
                }
            }
            AsyncRequest::ReferenceAcquire { .. } => Err(failure("acquisition was not forwarded")),
            // Forwarded above, where its reply can carry installed resources.
            AsyncRequest::ReferenceCall { .. } => Err(failure("import call was not forwarded")),
            AsyncRequest::Wait { milliseconds } => {
                if milliseconds > WAIT_LIMIT_MILLISECONDS {
                    return Err(failure("timer exceeds the callback wait limit"));
                }
                // A timer cooperates with an already admitted callback; it grants no new
                // root authority. Logical root end revokes imports, but physical callbacks
                // still settle under their retained signal and the Store's callback bound.
                let signal = match scope {
                    Scope::Call(context) => context.signal(),
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
        };
        value.map(Into::into)
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

    /// `Context.get`: acquire a selected WIT Orb import for the active invocation.
    ///
    /// The host supplies the root principal, the provider service and the installed requirement's
    /// API version and methods; the guest names only the binding and key. Native `Context::get`
    /// files the claim under the root, which releases it when the root ends. Readiness is awaited
    /// under this callback's cancellation, and a claim whose wait or authority fails is released
    /// before the error returns, so no handle is ever published for it.
    async fn acquire(
        &self,
        scope: &Scope,
        service: &str,
        key: Vec<String>,
    ) -> Result<crate::worker::Reply> {
        let context = Self::context(scope)?;
        // Only an admitted method call selects instances. A cleanup context (a resource `close`)
        // has no invocation; acquiring there would choose instances by the cleanup principal.
        context
            .invocation()
            .map_err(|_| failure("reference acquisition requires an admitted invocation"))?;
        let import = self
            .imports
            .get(service)
            .cloned()
            .ok_or_else(|| failure("reference is not a selected WIT Orb import"))?;
        let requirement = import
            .requirement
            .clone()
            .ok_or_else(|| failure("import has no installed Orb requirement"))?;
        let signal = context.signal();
        signal.check()?;
        let reference = context.get_wait(&import.service, key, requirement).await?;
        // Dropping an async import also drops this future while readiness is pending. The
        // root still owns the native claim, so dropping an Rc alone would leave it live.
        let mut pending = PendingAcquisition(Some(reference.clone()));
        let ready = {
            let ready = std::pin::pin!(reference.ready());
            let cancelled = std::pin::pin!(signal.cancelled());
            match futures::future::select(ready, cancelled).await {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => Err(failure("operation cancelled")),
            }
        };
        // The invocation or root may have ended while readiness was pending.
        if let Err(error) = ready.and_then(|()| context.check()) {
            return Err(pending.fail(error));
        }
        // A compiled Store shares one heap across environments. Its local view handles use
        // the Orb table's never-reused sequence; source contexts keep their own table.
        let table = if import.reference.is_some() {
            self.parent.as_ref().unwrap_or(&self.handles)
        } else {
            &self.handles
        };
        let id = table.allocate().map_err(|error| pending.fail(error))?;
        let value = encode(&id).map_err(|error| pending.fail(error))?;
        let view = import.reference.is_some();
        let mut references = table.references.borrow_mut();
        // Root claims are owned elsewhere; forget the names of those whose roots have ended.
        references.retain(|_, handle| !handle.claim.expired());
        references.insert(
            id,
            ReferenceHandle {
                claim: if view {
                    Claim::View {
                        reference: Rc::downgrade(&reference),
                        root: context.root,
                    }
                } else {
                    Claim::Root {
                        reference: Rc::downgrade(&reference),
                        root: context.root,
                    }
                },
                import,
            },
        );
        drop(references);
        let reply = crate::worker::Reply {
            value,
            delivery: view.then(|| Delivery::new(table, id)),
            stream: None,
        };
        pending.commit();
        Ok(reply)
    }

    /// Resolve a reference handle and its WIT binding for the current callback.
    ///
    /// Declared handles come from this environment or its issued parent. A root handle resolves
    /// only in the table that issued it, for a callback of the root that owns its claim, and only
    /// while that root lasts: another root, even of the same principal, cannot use it.
    pub(super) fn reference(
        &self,
        scope: &Scope,
        owner: Option<ScopeId>,
        id: u32,
    ) -> Result<(Rc<Reference>, Rc<Import>)> {
        let handles = self.selected_handles(owner)?;
        let handle = handles
            .references
            .borrow()
            .get(&id)
            .cloned()
            .ok_or_else(|| failure("unknown reference handle"))?;
        let (reference, root, table) = match handle.claim {
            Claim::Declared(reference) => {
                if handle.import.reference.is_some() {
                    return Err(failure("a reference view requires a root-owned claim"));
                }
                return Ok((reference, handle.import));
            }
            Claim::Root { reference, root } => (reference, root, &self.handles),
            Claim::View { reference, root } => (
                reference,
                root,
                self.parent.as_ref().unwrap_or(&self.handles),
            ),
            Claim::Owned(_) => {
                return Err(failure(
                    "an owned resource handle is not a service reference",
                ));
            }
        };
        let current = match scope {
            Scope::Call(context) => Some(context.root),
            _ => None,
        };
        if !Rc::ptr_eq(handles, table) || current != Some(root) {
            return Err(failure("reference belongs to another root"));
        }
        let reference = reference
            .upgrade()
            .ok_or_else(|| failure("reference expired with its root"))?;
        Ok((reference, handle.import))
    }
}

/// Release an acquisition abandoned before a reply can take responsibility for its claim.
struct PendingAcquisition(Option<Rc<Reference>>);

impl Drop for PendingAcquisition {
    fn drop(&mut self) {
        if let Some(reference) = self.0.take() {
            let _ = reference.release();
        }
    }
}

impl PendingAcquisition {
    /// Transfer responsibility to the root and, for a compiled view, the reply's delivery guard.
    fn commit(mut self) {
        self.0.take();
    }

    /// Release once and preserve the original failure, including any cleanup failure.
    fn fail(&mut self, mut error: tembra::Error) -> tembra::Error {
        if let Some(reference) = self.0.take()
            && let Err(release) = reference.release()
        {
            error.message = format!(
                "{}; releasing the claim also failed: {release}",
                error.message
            );
        }
        error
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
