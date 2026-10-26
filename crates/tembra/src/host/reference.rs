//! Native reference claims and typed calls.

use super::*;

/// A claim is released on drop. A running callback has its own physical kernel pin.
pub struct Reference {
    pub(super) host: Host,
    pub(super) id: core::Id,
    pub(super) object: core::Id,
    pub(super) released: Cell<bool>,
}

impl Reference {
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
        if !self.released.replace(true) {
            self.host.kernel().release(self.id)?;
            self.host.kernel().forget_reference(self.id)?;
            self.host.flush();
        }
        Ok(())
    }

    /// Waits for the kernel's activation program, including every resource opener.
    pub async fn ready(&self) -> Result<()> {
        let ready = self
            .host
            .0
            .objects
            .borrow()
            .get(&self.object)
            .and_then(|object| object.activation.clone());
        if let Some(ready) = ready {
            ready.await?;
        }
        match self.host.kernel().status(self.id)? {
            core::Status::Live => Ok(()),
            _ => Err(Error::invalid(Reason::NotReady, "object is not live")),
        }
    }

    /// Execute one typed local call while retaining physical ownership.
    pub async fn call<A: 'static, R: 'static>(
        &self,
        context: &Context,
        method: &str,
        arguments: A,
    ) -> Result<R> {
        if !self.host.same(&context.host) {
            return Err(Error::invalid(Reason::Context, "foreign host context"));
        }
        context.check()?;
        self.ready().await?;
        context.check()?;
        let call = self
            .host
            .kernel()
            .begin_call(self.id, context.root, method)?;
        let selected = {
            let objects = self.host.0.objects.borrow();
            objects.get(&call.object).and_then(|object| {
                object
                    .implementation
                    .local
                    .get(method)
                    .map(|handle| (object.environment.clone(), handle.clone()))
            })
        };
        let Some((environment, handle)) = selected else {
            self.host.kernel().finish_call(call.id)?;
            return Err(Error::invalid(Reason::NativeMethod, method));
        };
        let host = self.host.clone();
        let entered = host.context(call.root, Some(call.clone()))?;
        let receive = self.host.spawn(async move {
            let result = handle(environment, entered, Box::new(arguments)).await;
            host.kernel().finish_call(call.id)?;
            host.flush();
            result
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
