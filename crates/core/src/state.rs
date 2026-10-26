use crate::Reason;
use crate::{
    Code, Error, Host, Id, Result, Version,
    host::{conflict, gone},
};

/// Immutable metadata. Values remain owned by their language adapter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
    /// Attach initial state once while the object is loading.
    pub fn initialize_state(&self, object: Id, value: Id, version: Version) -> Result<Snapshot> {
        if value == 0 || version == 0 {
            return Err(Error::invalid(
                Reason::State,
                "value and state version must be nonzero",
            ));
        }
        let mut inner = self.inner.borrow_mut();
        let object = inner
            .objects
            .get_mut(&object)
            .ok_or_else(|| gone(Reason::Object))?;
        if object.phase != crate::Status::Loading || object.state.is_some() {
            return Err(conflict(Reason::StateInitialized));
        }
        let snapshot = Snapshot {
            value,
            version,
            revision: 0,
        };
        object.state = Some(snapshot);
        Ok(snapshot)
    }

    /// Read state metadata without decoding the adapter-owned value.
    pub fn snapshot(&self, object: Id) -> Result<Snapshot> {
        self.inner
            .borrow()
            .objects
            .get(&object)
            .and_then(|o| o.state)
            .ok_or_else(|| gone(Reason::State))
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
            return Err(conflict(Reason::StateRevision));
        }
        if value == 0 {
            return Err(Error::invalid(
                Reason::Value,
                "zero is not a value identity",
            ));
        }
        Ok(PreparedState {
            host: self.clone(),
            call,
            object,
            expected,
            value,
        })
    }

    /// Check and adopt a candidate against its expected revision.
    pub fn adopt_state(&self, call: Id, object: Id, expected: u64, value: Id) -> Result<Snapshot> {
        self.prepare_state(call, object, expected, value)?.adopt()
    }

    /// Require a live owner call at the object's accepted version.
    fn check_state_call(&self, call: Id, object: Id) -> Result<()> {
        let inner = self.inner.borrow();
        let call = inner.calls.get(&call).ok_or_else(|| gone(Reason::Call))?;
        inner.check_root(call.root)?;
        if call.object != object {
            return Err(Error::new(
                Code::Forbidden,
                Reason::StateOwner,
                "only an entered owner invocation may mutate state",
            ));
        }
        let record = inner
            .objects
            .get(&object)
            .ok_or_else(|| gone(Reason::Object))?;
        if call.code_version < record.min_version {
            return Err(conflict(Reason::UpgradeConflict));
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
            .ok_or_else(|| gone(Reason::Object))?;
        let previous = object.state.ok_or_else(|| gone(Reason::State))?;
        if previous.revision != self.expected {
            return Err(conflict(Reason::StateRevision));
        }
        if previous.value == self.value {
            return Ok(previous);
        }
        let revision = previous
            .revision
            .checked_add(1)
            .ok_or_else(|| conflict(Reason::RevisionExhausted))?;
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
