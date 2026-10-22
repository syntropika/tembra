use super::*;

/// Weak access to metadata retained by one managed implementation declaration.
///
/// Clones retain neither the Host, receiver, implementation nor metadata. This handle
/// creates no reference claim or invocation lease and grants no dispatch authority.
pub struct WeakImplementationOwner<T: 'static> {
    pub(super) host: Weak<HostInner>,
    pub(super) object: Weak<ObjectRecord>,
    pub(super) value: Weak<T>,
}

impl<T: 'static> Clone for WeakImplementationOwner<T> {
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            object: self.object.clone(),
            value: self.value.clone(),
        }
    }
}

impl<T: 'static> WeakImplementationOwner<T> {
    /// Borrows retained metadata through this Context's exact installed implementation.
    ///
    /// Unchanged implementation aliases can share metadata across admitted epochs. A new
    /// implementation must retain this exact value to grant access; matching Rust types or
    /// descriptor fingerprints do not suffice. Descendant Context scopes are supported.
    /// The returned Rc can outlive this check, so every later access must check again.
    ///
    /// # Errors
    /// Returns Host/Context failures, `not_ready/factory` during factory recording,
    /// `gone/implementation_owner` for an ended receiver or value, or `superseded/code_version`
    /// when the exact epoch or implementation no longer owns this value. Refusal mutates
    /// neither the receiver nor metadata and creates no physical invocation ownership.
    pub fn get(&self, context: &Context) -> Result<Rc<T>> {
        let host = self.host.upgrade().ok_or_else(|| {
            RuntimeError::unavailable("closed", "implementation Host no longer exists")
        })?;
        context.check(host.id)?;
        if host.in_factory.get() > 0 {
            return Err(RuntimeError::new(
                crate::ErrorCode::NotReady,
                "factory",
                "implementation metadata cannot be read in a factory",
            ));
        }
        host.epochs.exact(context.code_version())?;
        let gone = || {
            RuntimeError::new(
                crate::ErrorCode::Gone,
                "implementation_owner",
                "implementation metadata owner has ended",
            )
        };
        let object = self.object.upgrade().ok_or_else(gone)?;
        if object.phase.get() != Phase::Live {
            return Err(gone());
        }
        if context.code_version() < object.min_code_version.get() {
            return Err(crate::epoch::superseded());
        }
        let implementation = object
            .installed
            .borrow()
            .get(&context.code_version())
            .map(|installed| installed.implementation.clone())
            .ok_or_else(crate::epoch::superseded)?;
        let value = self.value.upgrade().ok_or_else(gone)?;
        if !implementation.retains_owner(&value) {
            return Err(crate::epoch::superseded());
        }
        Ok(value)
    }
}
