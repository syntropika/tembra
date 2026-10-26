//! Execute native factories and kernel-owned activation programs.

use super::*;

impl Host {
    /// Used by a foreign language adapter after the shared kernel reserves the object.
    pub fn activate(&self, object: core::Id, orb: core::Id, service: &str) -> Result<core::Id> {
        if let Some(object) = self.0.objects.borrow().get(&object) {
            return Ok(object.scope);
        }
        let implementation = self
            .0
            .orbs
            .borrow()
            .get(&orb)
            .and_then(|services| services.get(service))
            .cloned()
            .ok_or_else(|| Error::invalid(Reason::NativeService, service))?;
        let resources = Rc::new(RefCell::new(Vec::new()));
        let declaration = Declaration {
            host: self.clone(),
            object,
            resources: resources.clone(),
        };
        let result = (implementation.factory)(declaration);
        let scope = self.kernel().allocate_handle()?;
        self.kernel().register_scope(core::Scope {
            id: scope,
            owner: core::Owner::Object(object),
            orb,
            parents: Vec::new(),
            dependencies: Vec::new(),
            resources: resources.borrow().clone(),
            state_version: None,
            migrate: false,
            active: false,
            closing: false,
        })?;
        self.0.scope_objects.borrow_mut().insert(scope, object);
        self.0.objects.borrow_mut().insert(
            object,
            Object {
                environment: result.as_ref().cloned().unwrap_or_else(|_| Rc::new(())),
                implementation,
                scope,
                activation: None,
            },
        );
        result?;
        Ok(scope)
    }

    /// Release native object storage after its scope cleanup settles.
    pub async fn dispose(&self, object: core::Id) -> Result<()> {
        let scopes = self.kernel().scopes_for_owner(core::Owner::Object(object));
        if scopes.is_empty() {
            self.0.objects.borrow_mut().remove(&object);
            return Ok(());
        }
        let program = self.kernel().cleanup_scopes(scopes, None)?;
        self.run(program)?.await
    }
}
