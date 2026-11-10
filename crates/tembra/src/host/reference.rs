//! Native reference claims and typed calls.

use super::*;

type Boundary<A, R> = (
    Box<dyn FnOnce(&A) -> Result<Value>>,
    Box<dyn FnOnce(Value) -> Result<R>>,
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
    pub async fn call<A: 'static, R: 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
    ) -> Result<R> {
        self.invoke(context, method, arguments, None, false).await
    }

    /// Execute a local or foreign call, converting values only when crossing languages.
    /// JavaScript arguments use tuples or vectors to represent the method's argument list.
    pub async fn call_portable<A: Serialize + 'static, R: serde::de::DeserializeOwned + 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
    ) -> Result<R> {
        let boundary: Boundary<A, R> = (
            Box::new(|input| {
                serde_json::to_value(input)
                    .map_err(|error| Error::invalid(Reason::Arguments, error.to_string()))
            }),
            Box::new(|output| {
                serde_json::from_value(output)
                    .map_err(|error| Error::invalid(Reason::Result, error.to_string()))
            }),
        );
        self.invoke(context, method, arguments, Some(boundary), false)
            .await
    }

    /// Dispatch an already encoded foreign value through the provider's portable entrypoint.
    pub async fn call_value(
        &self,
        context: &Context,
        method: &str,
        arguments: Value,
    ) -> Result<Value> {
        self.invoke(
            context,
            method,
            arguments,
            Some((Box::new(|value| Ok(value.clone())), Box::new(Ok))),
            true,
        )
        .await
    }

    async fn invoke<A: 'static, R: 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
        boundary: Option<Boundary<A, R>>,
        portable_input: bool,
    ) -> Result<R> {
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
        )?;
        let isolated = boundary.is_some()
            && !portable_input
            && self.host.kernel().orb(call.orb)?.declaration.isolated;
        let portable = if isolated {
            self.host
                .0
                .objects
                .borrow()
                .get(&call.object)
                .and_then(|object| object.versions.get(&call.orb))
                .and_then(|object| {
                    object
                        .implementation
                        .portable
                        .get(method)
                        .map(|handle| (object.environment.clone(), handle.clone()))
                })
        } else {
            None
        };
        let selected = {
            let objects = self.host.0.objects.borrow();
            objects
                .get(&call.object)
                .and_then(|object| object.versions.get(&call.orb))
                .and_then(|object| {
                    if portable_input {
                        let portable = object.implementation.portable.get(method)?.clone();
                        let handle: crate::orb::LocalMethod =
                            Rc::new(move |environment, context, arguments| {
                                let portable = portable.clone();
                                async move {
                                    let arguments =
                                        arguments.downcast::<Value>().map_err(|_| type_error())?;
                                    Ok(Box::new(portable(environment, context, *arguments).await?)
                                        as Box<dyn std::any::Any>)
                                }
                                .boxed_local()
                            });
                        return Some((object.environment.clone(), handle));
                    }
                    object
                        .implementation
                        .local
                        .get(method)
                        .map(|handle| (object.environment.clone(), handle.clone()))
                })
        };
        // A host-local method refuses every portable caller, whichever boundary it uses.
        let local_only = boundary.is_some()
            && self
                .host
                .0
                .objects
                .borrow()
                .get(&call.object)
                .and_then(|object| object.versions.get(&call.orb))
                .is_some_and(|object| object.implementation.local_only(method));
        let method_name = method.to_owned();
        let host = self.host.clone();
        let entered = host.context(call.root, Some(call.clone()))?;
        let receive = self.host.spawn_managed(call.id,async move {
            if local_only {
                return Err(Error::invalid(Reason::LocalValue, format!("{method_name} is a host-local method without a portable entrypoint")));
            }
            if let Some((environment, handle))=portable {
                let (encode, decode) = boundary.ok_or_else(|| Error::invalid(Reason::LocalValue, "isolated calls require a portable boundary"))?;
                let input = encode(&arguments)?;
                let result = decode(handle(environment, entered, input).await?)?;
                Ok(Box::new(result) as Box<dyn std::any::Any>)
            } else if let Some((environment, handle))=selected {
                handle(environment, entered, Box::new(arguments)).await
            } else if !host.has_orb(call.orb) {
                let (encode,decode)=boundary.ok_or_else(||Error::invalid(Reason::LocalValue,"use call_portable for a foreign service"))?;
                let value=encode(&arguments)?;
                let result=host.foreign(serde_json::json!({"kind":"invoke","call":call.id,"frame":call.context,"input":value})).await?;
                let result=decode(result)?;
                Ok(Box::new(result) as Box<dyn std::any::Any>)
            } else { Err(Error::invalid(Reason::NativeMethod, method_name)) }
        })?;
        let result = receive.await.map_err(|_| {
            Error::invalid(Reason::Executor, "executor stopped before callback settled")
        })??;
        result
            .downcast::<R>()
            .map(|value| *value)
            .map_err(|_| type_error())
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
