use super::*;

impl Host {
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
            return Err(Error::new(Code::Closed, "host", "host is closed"));
        }
        let version = inner.version;
        inner.root(version, principal.into(), true)
    }

    pub fn check_root(&self, root: Id) -> Result<()> {
        self.inner.borrow().check_root(root).map(|_| ())
    }

    pub fn end_root(&self, root: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let record = inner.roots.get_mut(&root).ok_or_else(|| gone("root"))?;
        record.active = false;
        if record.calls == 0 {
            inner.remove_root(root);
        }
        Ok(())
    }

    pub fn cancel_root(&self, root: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let record = inner.roots.get_mut(&root).ok_or_else(|| gone("root"))?;
        if !record.cancelled {
            record.cancelled = true;
            inner.actions.push_back(Action::CancelRoot(root));
        }
        Ok(())
    }

    pub fn root_principal(&self, root: Id) -> Result<String> {
        Ok(self.inner.borrow().check_root(root)?.principal.clone())
    }

    /// Admits one physical call against its root's retained implementation version.
    pub fn begin_call(&self, reference: Id, root: Id, method: &str) -> Result<Invocation> {
        let mut inner = self.inner.borrow_mut();
        let context = inner.check_root(root)?;
        if inner.closing && !context.cleanup {
            return Err(Error::new(Code::Closed, "host", "host is closing"));
        }
        let version = context.version;
        let reference = inner
            .references
            .get(&reference)
            .ok_or_else(|| gone("reference"))?;
        if reference.released {
            return Err(Error::new(
                Code::Released,
                "reference",
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
                "method",
                "method is not exposed",
            ));
        }
        if !reference.methods.is_empty() && !reference.methods.contains(method) {
            return Err(Error::new(
                Code::Forbidden,
                "method",
                "method was not required",
            ));
        }
        let record = inner.objects.get(&object).ok_or_else(|| gone("object"))?;
        if record.phase != Status::Live || record.disposing {
            return Err(Error::new(
                Code::Unavailable,
                "not_ready",
                "object is not live",
            ));
        }
        if version < record.min_version {
            return Err(conflict("upgrade_conflict"));
        }
        if record
            .gate
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&root))
        {
            return Err(conflict("upgrade_conflict"));
        }
        let (facet, service) = inner.find_service(&record.address.service, version)?;
        if !service.methods.contains(method) {
            return Err(Error::invalid("method", "method is not declared"));
        }
        let facet_owner = inner.check_facet(facet, root, version)?;
        let id = inner.id()?;
        let call = Invocation {
            id,
            root,
            object,
            facet,
            code_version: version,
            method: method.into(),
        };
        inner.facet_roots.insert(facet_owner, root);
        inner.owner_calls.insert(Owner::Host, id);
        inner.owner_calls.insert(Owner::Facet(facet_owner), id);
        inner.owner_calls.insert(Owner::Object(object), id);
        inner.object_roots.insert(object, root);
        inner.calls.insert(id, call.clone());
        let context = inner.roots.get_mut(&root).expect("checked root");
        context.calls += 1;
        context.touched.insert(object);
        context.facets.insert(facet_owner);
        inner
            .objects
            .get_mut(&object)
            .expect("checked object")
            .calls += 1;
        Ok(call)
    }

    /// Marks actual callback settlement, independently from logical cancellation or caller release.
    pub fn finish_call(&self, call: Id) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        let call = inner.calls.remove(&call).ok_or_else(|| gone("call"))?;
        let facet_owner = inner.facets[&call.facet].owner;
        for owner in [
            Owner::Host,
            Owner::Facet(facet_owner),
            Owner::Object(call.object),
        ] {
            inner.owner_calls.remove(&owner, call.id);
            inner.settled_owner(owner);
        }
        let root = inner.roots.get_mut(&call.root).expect("call retains root");
        root.calls -= 1;
        if root.calls == 0 && !root.active {
            inner.remove_root(call.root);
        }
        if call.object != 0 {
            inner
                .objects
                .get_mut(&call.object)
                .expect("call retains object")
                .calls -= 1;
            inner.maybe_dispose(call.object);
        }
        Ok(())
    }

    pub fn invocation(&self, call: Id) -> Result<Invocation> {
        self.inner
            .borrow()
            .calls
            .get(&call)
            .cloned()
            .ok_or_else(|| gone("call"))
    }

    /// Enters a facet-owned operation such as a hook contribution without exposing a service.
    pub fn begin_operation(
        &self,
        facet: Id,
        root: Id,
        operation: Id,
        object: Option<Id>,
    ) -> Result<Invocation> {
        let mut inner = self.inner.borrow_mut();
        let context = inner.check_root(root)?;
        let version = context.version;
        let snapshot = inner
            .versions
            .get(&version)
            .ok_or_else(|| conflict("superseded"))?;
        if inner
            .facets
            .get(&facet)
            .is_none_or(|record| snapshot.get(&record.declaration.name) != Some(&facet))
            || operation == 0
        {
            return Err(gone("operation"));
        }
        let facet_owner = inner.check_facet(facet, root, version)?;
        if let Some(object) = object {
            let record = inner.objects.get(&object).ok_or_else(|| gone("object"))?;
            if record.phase != Status::Live || record.disposing {
                return Err(gone("object"));
            }
            if version < record.min_version
                || record
                    .gate
                    .as_ref()
                    .is_some_and(|roots| !roots.contains(&root))
            {
                return Err(conflict("upgrade_conflict"));
            }
            let (expected, _) = inner.find_service(&record.address.service, version)?;
            if expected != facet {
                return Err(conflict("operation_owner"));
            }
        }
        let id = inner.id()?;
        let invocation = Invocation {
            id,
            root,
            object: object.unwrap_or(0),
            facet,
            code_version: version,
            method: operation.to_string(),
        };
        inner.facet_roots.insert(facet_owner, root);
        inner.owner_calls.insert(Owner::Host, id);
        inner.owner_calls.insert(Owner::Facet(facet_owner), id);
        if let Some(object) = object {
            inner.owner_calls.insert(Owner::Object(object), id);
            inner.object_roots.insert(object, root);
            inner
                .roots
                .get_mut(&root)
                .expect("checked root")
                .touched
                .insert(object);
        }
        inner.calls.insert(id, invocation.clone());
        inner.roots.get_mut(&root).expect("checked root").calls += 1;
        inner
            .roots
            .get_mut(&root)
            .expect("checked root")
            .facets
            .insert(facet_owner);
        if let Some(object) = object {
            inner
                .objects
                .get_mut(&object)
                .expect("checked object")
                .calls += 1;
        }
        Ok(invocation)
    }
}
