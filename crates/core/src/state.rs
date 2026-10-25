use crate::{
    Code, Error, Host, Id, Result, Version,
    host::{conflict, gone},
};

/// Immutable metadata. Values remain owned by their language adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub value: Id,
    pub revision: u64,
    pub version: Version,
}

/// A private candidate tied to the exact invocation, object, revision and issuing host.
pub struct PreparedState {
    host: Host,
    call: Id,
    object: Id,
    expected: u64,
    value: Id,
}

impl Host {
    pub fn initialize_state(&self, object: Id, value: Id, version: Version) -> Result<Snapshot> {
        if value == 0 || version == 0 {
            return Err(Error::invalid(
                "state",
                "value and state version must be nonzero",
            ));
        }
        let mut inner = self.inner.borrow_mut();
        let object = inner
            .objects
            .get_mut(&object)
            .ok_or_else(|| gone("object"))?;
        if object.phase != crate::Status::Loading || object.state.is_some() {
            return Err(conflict("state_initialized"));
        }
        let snapshot = Snapshot {
            value,
            version,
            revision: 0,
        };
        object.state = Some(snapshot);
        Ok(snapshot)
    }

    pub fn snapshot(&self, object: Id) -> Result<Snapshot> {
        self.inner
            .borrow()
            .objects
            .get(&object)
            .and_then(|o| o.state)
            .ok_or_else(|| gone("state"))
    }

    /// Captures the revision before adapter code prepares and validates a candidate.
    pub fn prepare_state(
        &self,
        call: Id,
        object: Id,
        expected: u64,
        value: Id,
    ) -> Result<PreparedState> {
        self.check_state_call(call, object)?;
        let snapshot = self.snapshot(object)?;
        if snapshot.revision != expected {
            return Err(conflict("state_revision"));
        }
        if value == 0 {
            return Err(Error::invalid("value", "zero is not a value identity"));
        }
        Ok(PreparedState {
            host: self.clone(),
            call,
            object,
            expected,
            value,
        })
    }

    pub fn adopt_state(&self, call: Id, object: Id, expected: u64, value: Id) -> Result<Snapshot> {
        self.prepare_state(call, object, expected, value)?.adopt()
    }

    fn check_state_call(&self, call: Id, object: Id) -> Result<()> {
        let inner = self.inner.borrow();
        let call = inner.calls.get(&call).ok_or_else(|| gone("call"))?;
        inner.check_root(call.root)?;
        if call.object != object {
            return Err(Error::new(
                Code::Forbidden,
                "state_owner",
                "only an entered owner invocation may mutate state",
            ));
        }
        let record = inner.objects.get(&object).ok_or_else(|| gone("object"))?;
        if call.code_version < record.min_version {
            return Err(conflict("upgrade_conflict"));
        }
        Ok(())
    }
}

impl PreparedState {
    /// Atomically adopts a previously adapter-validated value.
    ///
    /// # Guarantees
    /// Failure does not change the snapshot or emit a notification. Validation belongs to the
    /// facet; this method checks ownership, scope, execution version and optimistic revision.
    pub fn adopt(self) -> Result<Snapshot> {
        self.host.check_state_call(self.call, self.object)?;
        let mut inner = self.host.inner.borrow_mut();
        let object = inner
            .objects
            .get_mut(&self.object)
            .ok_or_else(|| gone("object"))?;
        let previous = object.state.ok_or_else(|| gone("state"))?;
        if previous.revision != self.expected {
            return Err(conflict("state_revision"));
        }
        if previous.value == self.value {
            return Ok(previous);
        }
        let revision = previous
            .revision
            .checked_add(1)
            .ok_or_else(|| conflict("revision_exhausted"))?;
        let snapshot = Snapshot {
            value: self.value,
            revision,
            version: previous.version,
        };
        object.state = Some(snapshot);
        inner
            .actions
            .push_back(crate::Action::DropValue(previous.value));
        inner.actions.push_back(crate::Action::StateChanged {
            object: self.object,
            revision,
        });
        Ok(snapshot)
    }
}
