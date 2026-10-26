use crate::{Context, Error, Host, Result, core};
use serde::Serialize;
use std::{marker::PhantomData, rc::Rc};

type Validator<T> = Rc<dyn Fn(&T) -> Result<()>>;

/// Immutable snapshots stay in Rust. Validation runs before the kernel adopts their identity.
pub struct State<T> {
    host: Host,
    object: core::Id,
    validate: Validator<T>,
    marker: PhantomData<T>,
}
impl<T> Clone for State<T> {
    /// Share the same state owner without copying its value.
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            object: self.object,
            validate: self.validate.clone(),
            marker: PhantomData,
        }
    }
}
impl<T: Serialize + 'static> State<T> {
    /// Bind typed state access to one kernel object.
    pub(crate) fn new(
        host: Host,
        object: core::Id,
        initial: T,
        version: u32,
        validate: impl Fn(&T) -> Result<()> + 'static,
    ) -> Result<Self> {
        validate(&initial)?;
        let value = host.store(initial, true)?;
        if let Err(error) = host.kernel().initialize_state(object, value, version) {
            host.drop_value(value);
            return Err(error);
        }
        Ok(Self {
            host,
            object,
            validate: Rc::new(validate),
            marker: PhantomData,
        })
    }

    /// Read the typed value identified by the current snapshot.
    pub fn value(&self) -> Result<Rc<T>> {
        self.host
            .value(self.host.kernel().snapshot(self.object)?.value)
    }

    /// Failure leaves the snapshot unchanged. A candidate based on an obsolete revision is refused.
    pub fn update(&self, context: &Context, change: impl FnOnce(&T) -> Result<T>) -> Result<Rc<T>> {
        let call = context.invocation()?;
        if !self.host.same(&context.host) || call.object != self.object {
            return Err(Error::new(
                core::Code::Forbidden,
                "state_owner",
                "context does not own this state",
            ));
        }
        context.check()?;
        let previous = self.host.kernel().snapshot(self.object)?;
        let current = self.host.value::<T>(previous.value)?;
        let candidate = change(&current)?;
        (self.validate)(&candidate)?;
        context.check()?;
        let value = self.host.store(candidate, true)?;
        if let Err(error) =
            self.host
                .kernel()
                .adopt_state(call.id, self.object, previous.revision, value)
        {
            self.host.drop_value(value);
            return Err(error);
        }
        let result = self.host.value(value);
        self.host.publish_state(self.object);
        self.host.flush();
        result
    }
}

impl<T: 'static> State<T> {
    /// Read or observe immutable snapshots owned by this state object.
    pub fn readable(&self) -> crate::Readable<T> {
        crate::Readable::new(self.host.clone(), self.object, None)
    }
}
