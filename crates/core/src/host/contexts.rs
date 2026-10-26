use super::*;
use crate::Reason;

impl Host {
    /// Pin the current implementation version for a new operation root.
    pub fn begin_root(&self, principal: impl Into<String>) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        inner.open()?;
        let version = inner.version;
        inner.root(version, principal.into(), false)
    }

    /// Cleanup roots may call already-live dependencies during orderly shutdown.
    pub fn cleanup_root(&self, principal: impl Into<String>) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        if inner.closed {
            return Err(Error::new(Code::Closed, Reason::Host, "host is closed"));
        }
        let version = inner.version;
        inner.root(version, principal.into(), true)
    }

    /// Require an active root that has not been cancelled.
    pub fn check_root(&self, root: Id) -> Result<()> {
        self.inner.borrow().check_root(root).map(|_| ())
    }

    /// End logical admission while physical calls retain the root.
    pub fn end_root(&self, root: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let record = inner
            .roots
            .get_mut(&root)
            .ok_or_else(|| gone(Reason::Root))?;
        record.active = false;
        let leases = if record.calls == 0 {
            inner.remove_root(root)
        } else {
            Vec::new()
        };
        drop(inner);
        drop(leases);
        Ok(())
    }

    /// Emit cancellation once without ending physical calls.
    pub fn cancel_root(&self, root: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let record = inner
            .roots
            .get_mut(&root)
            .ok_or_else(|| gone(Reason::Root))?;
        if !record.cancelled {
            record.cancelled = true;
            inner.actions.push_back(Action::CancelRoot(root));
        }
        Ok(())
    }

    /// Read the identity retained by an active root.
    pub fn root_principal(&self, root: Id) -> Result<String> {
        Ok(self.inner.borrow().check_root(root)?.principal.clone())
    }

    /// Admits one physical call against its root's retained implementation version.
    /// Refuses roots older than the object's minimum version before retaining a call.
    pub fn begin_call(&self, reference: Id, root: Id, method: &str) -> Result<Invocation> {
        self.begin_call_with_context(reference, root, method, 0)
    }

    /// Admit one call with its own immutable frame.
    pub fn begin_call_with_context(
        &self,
        reference: Id,
        root: Id,
        method: &str,
        frame: Id,
    ) -> Result<Invocation> {
        let mut inner = self.inner.borrow_mut();
        inner.check_frame(root, frame)?;
        let context = inner.check_root(root)?;
        if inner.closing && !context.cleanup {
            return Err(Error::new(Code::Closed, Reason::Host, "host is closing"));
        }
        let version = context.version;
        let reference = inner
            .references
            .get(&reference)
            .ok_or_else(|| gone(Reason::Reference))?;
        if reference.released {
            return Err(Error::new(
                Code::Released,
                Reason::Reference,
                "reference is released",
            ));
        }
        let object = reference.object;
        #[cfg(feature = "views")]
        if reference
            .allowed
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(method))
        {
            return Err(Error::new(
                Code::Forbidden,
                Reason::Method,
                "method is not exposed",
            ));
        }
        if !reference.methods.is_empty() && !reference.methods.contains(method) {
            return Err(Error::new(
                Code::Forbidden,
                Reason::Method,
                "method was not required",
            ));
        }
        let record = inner
            .objects
            .get(&object)
            .ok_or_else(|| gone(Reason::Object))?;
        if record.phase != Status::Live || record.disposing {
            return Err(Error::new(
                Code::Unavailable,
                Reason::NotReady,
                "object is not live",
            ));
        }
        if version < record.min_version {
            return Err(conflict(Reason::UpgradeConflict));
        }
        if record
            .gate
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&root))
        {
            return Err(conflict(Reason::UpgradeConflict));
        }
        let (orb, service) = inner.find_service(&record.address.service, version)?;
        if !service.methods.contains(method) {
            return Err(Error::invalid(Reason::Method, "method is not declared"));
        }
        let orb_owner = inner.check_orb(orb, root, version)?;
        let id = inner.id()?;
        let call = Invocation {
            context: frame,
            id,
            root,
            object,
            orb,
            code_version: version,
            method: method.into(),
        };
        Ok(inner.retain_call(call, orb_owner))
    }

    /// Marks actual callback settlement, independently from logical cancellation or caller release.
    pub fn finish_call(&self, call: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let call = inner
            .calls
            .remove(&call)
            .ok_or_else(|| gone(Reason::Call))?;
        let mut leases = Vec::new();
        for frame in inner.call_contexts.remove(&call.id).unwrap_or_default() {
            inner.release_frame(frame, &mut leases);
        }
        inner.release_frame(call.context, &mut leases);
        let orb_owner = inner.orbs[&call.orb].owner;
        for owner in [
            Owner::Host,
            Owner::Orb(orb_owner),
            Owner::Object(call.object),
        ] {
            inner.owner_calls.remove(&owner, call.id);
            inner.settled_owner(owner);
        }
        let root = inner.roots.get_mut(&call.root).expect("call retains root");
        root.calls -= 1;
        if root.calls == 0 && !root.active {
            leases.extend(inner.remove_root(call.root));
        }
        if call.object != 0 {
            inner
                .objects
                .get_mut(&call.object)
                .expect("call retains object")
                .calls -= 1;
            inner.maybe_dispose(call.object);
        }
        drop(inner);
        drop(leases);
        Ok(())
    }

    /// Read the record for a physically active call.
    pub fn invocation(&self, call: Id) -> Result<Invocation> {
        self.inner
            .borrow()
            .calls
            .get(&call)
            .cloned()
            .ok_or_else(|| gone(Reason::Call))
    }

    /// Enters an orb-owned operation such as a hook contribution without exposing a service.
    pub fn begin_operation(
        &self,
        orb: Id,
        root: Id,
        operation: Id,
        object: Option<Id>,
    ) -> Result<Invocation> {
        self.begin_operation_with_context(orb, root, operation, object, 0)
    }

    /// Admit an orb operation with its caller's frame.
    pub fn begin_operation_with_context(
        &self,
        orb: Id,
        root: Id,
        operation: Id,
        object: Option<Id>,
        frame: Id,
    ) -> Result<Invocation> {
        let mut inner = self.inner.borrow_mut();
        inner.check_frame(root, frame)?;
        let context = inner.check_root(root)?;
        let version = context.version;
        let snapshot = inner
            .versions
            .get(&version)
            .ok_or_else(|| conflict(Reason::Superseded))?;
        if inner
            .orbs
            .get(&orb)
            .is_none_or(|record| snapshot.get(&record.declaration.name) != Some(&orb))
            || operation == 0
        {
            return Err(gone(Reason::Operation));
        }
        let orb_owner = inner.check_orb(orb, root, version)?;
        if let Some(object) = object {
            let record = inner
                .objects
                .get(&object)
                .ok_or_else(|| gone(Reason::Object))?;
            if record.phase != Status::Live || record.disposing {
                return Err(gone(Reason::Object));
            }
            if version < record.min_version
                || record
                    .gate
                    .as_ref()
                    .is_some_and(|roots| !roots.contains(&root))
            {
                return Err(conflict(Reason::UpgradeConflict));
            }
            let (expected, _) = inner.find_service(&record.address.service, version)?;
            if expected != orb {
                return Err(conflict(Reason::OperationOwner));
            }
        }
        let id = inner.id()?;
        let invocation = Invocation {
            context: frame,
            id,
            root,
            object: object.unwrap_or(0),
            orb,
            code_version: version,
            method: operation.to_string(),
        };
        Ok(inner.retain_call(invocation, orb_owner))
    }
}

impl Inner {
    /// Retain physical call ownership and its reverse indexes together.
    fn retain_call(&mut self, call: Invocation, orb_owner: Id) -> Invocation {
        self.orb_roots.insert(orb_owner, call.root);
        self.owner_calls.insert(Owner::Host, call.id);
        self.owner_calls.insert(Owner::Orb(orb_owner), call.id);
        if call.object != 0 {
            self.owner_calls.insert(Owner::Object(call.object), call.id);
            self.object_roots.insert(call.object, call.root);
            self.objects
                .get_mut(&call.object)
                .expect("checked object")
                .calls += 1;
        }
        let root = self.roots.get_mut(&call.root).expect("checked root");
        root.calls += 1;
        root.orbs.insert(orb_owner);
        if call.object != 0 {
            root.touched.insert(call.object);
        }
        self.retain_frame(call.context);
        self.calls.insert(call.id, call.clone());
        call
    }
}
