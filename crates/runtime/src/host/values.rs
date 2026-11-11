//! Native values cross a language only through the selected codec.

use super::*;

impl Host {
    /// Execute an admitted native call through its codec-compatible entrypoint.
    pub async fn invoke_portable(&self, call: core::Invocation, arguments: Value) -> Result<Value> {
        let (environment, handle) = {
            let objects = self.0.objects.borrow();
            let object = objects
                .get(&call.object)
                .and_then(|object| object.versions.get(&call.orb))
                .ok_or_else(|| Error::invalid(Reason::NativeObject, "object is not activated"))?;
            (
                object.environment.clone(),
                object
                    .implementation
                    .portable
                    .get(&call.method)
                    .cloned()
                    .ok_or_else(|| Error::invalid(Reason::NativeMethod, &call.method))?,
            )
        };
        let entered = self.context(call.root, Some(call))?;
        // JSON and RPC callers are data-only: a capability-bearing result, including a stream
        // end, is refused and closed. This boundary cannot hand out an early result, so it
        // returns, with a value or an error, only after the call's physical work has ended.
        let host = self.clone();
        self.spawn(async move {
            let (head, physical) = handle(
                environment,
                entered,
                crate::portable::Payload::data(arguments),
            )
            .await?
            .into_parts();
            let value = head.and_then(crate::portable::Payload::into_data);
            let settled = match physical {
                Some(physical) => physical.await,
                None => Ok(()),
            };
            match (value, settled) {
                (Ok(value), settled) => settled.map(|()| value),
                // The head's error is the one returned; a physical failure behind it would
                // otherwise be lost, so it is recorded like any other late physical failure.
                (Err(primary), Err(physical)) => {
                    host.0.errors.borrow_mut().push(physical);
                    Err(primary)
                }
                (Err(primary), Ok(())) => Err(primary),
            }
        })?
        .await
        .map_err(|_| Error::invalid(Reason::Executor, "native callback executor stopped"))?
    }

    /// Encode a value only if its declaration permits language crossing.
    pub fn portable_value(&self, id: core::Id) -> Result<Value> {
        let encoder = self
            .0
            .values
            .borrow()
            .get(&id)
            .and_then(|record| record.portable.clone())
            .ok_or_else(|| Error::invalid(Reason::LocalValue, "this Rust value is not portable"))?;
        encoder()
    }

    /// Retain a typed value with an optional lazy portable encoder.
    pub(crate) fn store<T: Serialize + 'static>(
        &self,
        value: T,
        portable: bool,
    ) -> Result<core::Id> {
        let id = self.kernel().allocate_handle()?;
        let value = Rc::new(value);
        let serialized = value.clone();
        let encoder: Option<Encoder> = portable.then(|| {
            Rc::new(move || {
                serde_json::to_value(&*serialized)
                    .map_err(|error| Error::invalid(Reason::Serialization, error.to_string()))
            }) as Encoder
        });
        self.0.values.borrow_mut().insert(
            id,
            ValueRecord {
                value,
                portable: encoder,
            },
        );
        Ok(id)
    }

    /// Resolve a retained native value with checked type conversion.
    pub(crate) fn value<T: 'static>(&self, id: core::Id) -> Result<Rc<T>> {
        self.0
            .values
            .borrow()
            .get(&id)
            .ok_or_else(type_error)?
            .value
            .clone()
            .downcast()
            .map_err(|_| type_error())
    }

    /// Release one native bridge value.
    pub fn drop_value(&self, id: core::Id) {
        self.0.values.borrow_mut().remove(&id);
    }
}
