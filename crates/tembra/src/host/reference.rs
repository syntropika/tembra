//! Native reference claims and typed calls.

use super::*;

use crate::portable::{FromPayload, IntoPayload, Origin, Payload};

/// Encoding and decoding of one portable call. Arguments move by value so that capabilities
/// can travel; decoding receives the calling claim to anchor owned results.
type Boundary<A, R> = (
    Box<dyn FnOnce(A) -> Result<Payload>>,
    Box<dyn FnOnce(Payload, &Origin) -> Result<R>>,
);

/// A claim is released on drop. A running callback has its own physical kernel pin.
pub struct Reference {
    pub(super) host: Host,
    pub(super) id: core::Id,
    pub(super) object: core::Id,
    pub(super) foreign_ready: Option<Shared<LocalBoxFuture<'static, Result<()>>>>,
    pub(super) released: Rc<crate::readable::ReadLifetime>,
}

impl Reference {
    /// Encode the provider's current state through its portable boundary while this claim is live.
    pub fn state_snapshot(&self) -> Result<(core::Snapshot, Value)> {
        if self.status()? != core::Status::Live {
            return Err(Error::invalid(Reason::NotReady, "reference is not live"));
        }
        let snapshot = self.host.kernel().snapshot(self.object)?;
        Ok((snapshot, self.host.portable_value(snapshot.value)?))
    }

    /// Read this reference's kernel identity.
    pub fn id(&self) -> core::Id {
        self.id
    }

    /// Read the object identity retained by this reference.
    pub fn object(&self) -> core::Id {
        self.object
    }

    /// Release this ownership claim once.
    pub fn release(&self) -> Result<()> {
        if !self.released.released.replace(true) {
            self.released.close();
            if self.foreign_ready.is_some() {
                self.host
                    .notify_foreign(super::signals::ForeignNotification::ReferenceReleased(
                        self.id,
                    ));
            }

            if !self.host.0.external_actions.get() {
                self.host.tick();
            }
            if self.host.kernel().status(self.id).is_err() {
                return Ok(());
            }
            self.host.kernel().release(self.id)?;
            self.host.kernel().forget_reference(self.id)?;
            self.host.flush();
        }
        Ok(())
    }

    /// Read this claim's lifecycle status, including deterministic release.
    pub fn status(&self) -> Result<core::Status> {
        if self.released.released.get() {
            Ok(core::Status::Released)
        } else {
            self.host.kernel().status(self.id)
        }
    }

    /// Waits for the kernel's activation program, including every resource opener.
    pub async fn ready(&self) -> Result<()> {
        self.activation().await?;
        match self.host.kernel().status(self.id)? {
            core::Status::Live => Ok(()),
            _ => Err(Error::invalid(Reason::NotReady, "object is not live")),
        }
    }

    async fn activation(&self) -> Result<()> {
        let ready = self
            .host
            .0
            .objects
            .borrow()
            .get(&self.object)
            .and_then(|object| object.activation.clone());
        let ready = ready.or_else(|| self.foreign_ready.clone());
        if let Some(ready) = ready {
            ready.await?;
        }
        Ok(())
    }

    /// Execute a typed local Rust call without requiring portable value traits.
    /// Arguments and results move as Rust values; nothing is serialized.
    pub async fn call<A: 'static, R: 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
    ) -> Result<R> {
        self.invoke(context, method, arguments, None).await
    }

    /// Call the provider's portable entrypoint, whatever language or isolation it has.
    /// JavaScript arguments use tuples or vectors to represent the method's argument list.
    /// Results are decoded inside the managed call, so an owned capability that cannot be
    /// delivered is closed rather than leaked.
    pub async fn call_portable<A: IntoPayload + 'static, R: FromPayload + 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
    ) -> Result<R> {
        let boundary: Boundary<A, R> = (Box::new(A::into_payload), Box::new(R::from_payload));
        self.invoke(context, method, arguments, Some(boundary))
            .await
    }

    /// Like [`Reference::call_portable`], but its caller keeps control of the admitted
    /// invocation: once admitted, it receives the [`IssuedCall`], which cancels exactly this call
    /// and waits for its physical end, and the call's result, which it may stop awaiting.
    pub async fn issue_portable<A: IntoPayload + 'static, R: FromPayload + 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
    ) -> Result<(IssuedCall, LocalBoxFuture<'static, Result<R>>)> {
        let boundary: Boundary<A, R> = (Box::new(A::into_payload), Box::new(R::from_payload));
        self.issue(context, method, arguments, Some(boundary)).await
    }

    /// Only `Owned` can originate implicit self. Public payload arguments cannot forward a
    /// provider's retained receiver into another call.
    pub(crate) async fn call_borrowed<R: FromPayload + 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: Payload,
    ) -> Result<R> {
        let (_, result) = self.issue_borrowed(context, method, arguments).await?;
        result.await
    }

    /// [`Reference::call_borrowed`] with control of the admitted invocation.
    pub(crate) async fn issue_borrowed<R: FromPayload + 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: Payload,
    ) -> Result<(IssuedCall, LocalBoxFuture<'static, Result<R>>)> {
        let boundary: Boundary<Payload, R> = (Box::new(Ok), Box::new(R::from_payload));
        self.issue(context, method, arguments, Some(boundary)).await
    }

    async fn invoke<A: 'static, R: 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
        boundary: Option<Boundary<A, R>>,
    ) -> Result<R> {
        let (_, result) = self.issue(context, method, arguments, boundary).await?;
        result.await
    }

    /// Admit one call issued from `context` and start it as managed work. The call's guard, not
    /// its caller, owns the invocation until it physically ends.
    async fn issue<A: 'static, R: 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
        boundary: Option<Boundary<A, R>>,
    ) -> Result<(IssuedCall, LocalBoxFuture<'static, Result<R>>)> {
        if !self.host.same(&context.host) {
            return Err(Error::invalid(Reason::Context, "foreign host context"));
        }
        context.check()?;
        self.activation().await?;
        context.check()?;
        let call = self.host.kernel().begin_call_with_context(
            self.id,
            context.root,
            method,
            context.frame(),
            context.issuer(),
        )?;
        let issued = IssuedCall {
            host: self.host.clone(),
            call: call.id,
        };
        let route = self.route(&call, method, boundary.is_some());
        let foreign = matches!(route, Route::Foreign);
        let origin = Origin::new(self.host.clone(), self.id, self.object, foreign, call.root);
        let method_name = method.to_owned();
        let host = self.host.clone();
        let entered = host.context(call.root, Some(call.clone()))?;
        let receive = self.host.spawn_managed_settling(call.id, async move {
            let result = match (route, boundary) {
                (Route::Portable(environment, handle), Some((encode, decode))) => {
                    let (arguments, receiver) = match encode(arguments).and_then(Payload::for_call)
                    {
                        Ok(prepared) => prepared,
                        Err(error) => return (Err(error), None),
                    };
                    let (head, physical) = match handle(environment, entered, arguments).await {
                        Ok(outcome) => outcome.into_parts(),
                        Err(error) => return (Err(error), None),
                    };
                    // The head is bound and decoded here, before it is published. A refused
                    // head is dropped, closing what it carries. Native decoding, or a refused
                    // attachment's close, may panic: only that step is caught, so the physical
                    // work and the receiver pin below are never lost with it.
                    let head = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        head.and_then(|payload| {
                            payload.bind_streams(&origin)?;
                            decode(payload, &origin)
                        })
                        .map(|value| Box::new(value) as Box<dyn std::any::Any>)
                    }))
                    .unwrap_or_else(|_| {
                        Err(Error::new(
                            core::Code::Unavailable,
                            "callback",
                            "native result decoding panicked",
                        ))
                    });
                    // Implicit self stays pinned until the call has physically ended; without
                    // remaining work the pin drops here, before the invocation finishes.
                    let physical = physical.map(|physical| {
                        async move {
                            let settled = physical.await;
                            drop(receiver);
                            settled
                        }
                        .boxed_local()
                    });
                    return (head, physical);
                }
                (Route::Local(environment, handle), None) => {
                    handle(environment, entered, Box::new(arguments)).await
                }
                (Route::Foreign, Some((encode, decode))) => {
                    async {
                        let value = encode(arguments)?.into_data()?;
                        let result = host
                            .foreign(serde_json::json!({
                                "kind": "invoke", "call": call.id, "frame": call.context, "input": value,
                            }))
                            .await?;
                        Ok(Box::new(decode(Payload::data(result), &origin)?) as Box<dyn std::any::Any>)
                    }
                    .await
                }
                (Route::Foreign, None) => Err(Error::invalid(
                    Reason::LocalValue,
                    "use call_portable for a foreign service",
                )),
                (Route::LocalOnly, _) => Err(Error::invalid(
                    Reason::LocalValue,
                    format!("{method_name} is a host-local method without a portable entrypoint"),
                )),
                _ => Err(Error::invalid(Reason::NativeMethod, method_name)),
            };
            (result, None)
        })?;
        let result = async move {
            let result = receive.await.map_err(|_| {
                Error::invalid(Reason::Executor, "executor stopped before callback settled")
            })??;
            result
                .downcast::<R>()
                .map(|value| *value)
                .map_err(|_| type_error())
        }
        .boxed_local();
        Ok((issued, result))
    }

    /// Select the admitted call's entrypoint. Portable calls always use the provider's portable
    /// entrypoint, even for an in-process native provider; typed calls use its local one.
    fn route(&self, call: &core::Invocation, method: &str, portable: bool) -> Route {
        if !self.host.has_orb(call.orb) {
            return Route::Foreign;
        }
        let objects = self.host.0.objects.borrow();
        let Some(object) = objects
            .get(&call.object)
            .and_then(|object| object.versions.get(&call.orb))
        else {
            return Route::Missing;
        };
        let environment = object.environment.clone();
        let implementation = &object.implementation;
        if portable {
            match implementation.portable.get(method) {
                Some(handle) => Route::Portable(environment, handle.clone()),
                // A host-local method refuses every portable caller.
                None if implementation.local_only(method) => Route::LocalOnly,
                None => Route::Missing,
            }
        } else {
            implementation
                .local
                .get(method)
                .map_or(Route::Missing, |handle| {
                    Route::Local(environment, handle.clone())
                })
        }
    }
}

/// A dependency call as its caller issued it: the caller may cancel exactly this invocation,
/// and wait until it has physically ended.
///
/// Cancellation is logical. The call, and the calls it issued, observe it through their
/// context's signal, and admit no further work; their root, their caller and sibling calls are
/// unaffected. The call keeps running, counted by the kernel and owned by its managed task,
/// until it physically ends, however long a callee that ignores the signal takes.
#[derive(Clone)]
pub struct IssuedCall {
    host: Host,
    call: core::Id,
}

impl IssuedCall {
    /// The admitted invocation's kernel identity.
    pub fn id(&self) -> core::Id {
        self.call
    }

    /// Cancel this invocation and the calls it issued. Doing so again, or after the call
    /// ended, does nothing.
    pub fn cancel(&self) {
        let Ok(cancelled) = self.host.kernel().cancel_call(self.call) else {
            return;
        };
        for call in cancelled {
            self.host.cancel_call_signal(call);
        }
        self.host.flush();
    }

    /// Resolve once the invocation has physically ended: its managed task finished the call.
    pub async fn settled(&self) {
        while self.host.kernel().invocation(self.call).is_ok() {
            let (send, receive) = oneshot::channel();
            self.host.0.settled.borrow_mut().push(send);
            let _ = receive.await;
        }
    }
}

/// The entrypoint an admitted call dispatches to.
enum Route {
    Portable(crate::orb::Data, crate::orb::PortableMethod),
    Local(crate::orb::Data, crate::orb::LocalMethod),
    Foreign,
    LocalOnly,
    Missing,
}

impl Host {
    /// A second claim with exactly the same admission as `reference`, owned by the caller.
    pub(crate) fn duplicate_reference(
        &self,
        reference: core::Id,
        object: core::Id,
    ) -> Result<Reference> {
        let id = self.kernel().duplicate(reference)?;
        Ok(Reference {
            host: self.clone(),
            id,
            object,
            foreign_ready: None,
            released: Rc::new(crate::readable::ReadLifetime::default()),
        })
    }
}

impl Drop for Reference {
    /// Release an unconsumed reference claim during destruction.
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            self.host.0.errors.borrow_mut().push(error);
        }
    }
}

impl Reference {
    /// Read native typed state and bind all its observers to this claim's lifetime.
    pub fn state<T: 'static>(&self) -> Result<crate::Readable<T>> {
        if self.status()? != core::Status::Live {
            return Err(Error::invalid(Reason::NotReady, "reference is not live"));
        }
        let view =
            crate::Readable::new(self.host.clone(), self.object, Some(self.released.clone()));
        view.value()?;
        Ok(view)
    }
}
