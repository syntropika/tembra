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
        let declaration = self.declaration(core::Owner::Object(object), orb);
        let result = (implementation.factory)(declaration.clone());
        let parents = self
            .0
            .orb_scopes
            .borrow()
            .get(&orb)
            .copied()
            .into_iter()
            .collect();
        let scope = self.register_declaration(&declaration, parents)?;
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
