//! Execute native factories and kernel-owned activation programs.

use super::*;

impl Host {
    /// Retain delayed factory admission as part of object readiness and host shutdown.
    pub(super) fn activate_deferred(
        &self,
        acquired: &core::Acquisition,
        service: String,
        before: crate::orb::BeforeCreate,
    ) -> Result<()> {
        let object = acquired.object;
        let orb = acquired.orb;
        let host = self.clone();
        let receive = self.spawn(async move {
            let result = std::panic::AssertUnwindSafe(async {
                let permit = before().await?;
                host.kernel().object_address(object)?;
                let scope = host.activate(object, orb, &service)?;
                drop(permit);
                let program = host.kernel().activate_scope(scope)?;
                host.run(program)?.await
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(Error::new(
                    core::Code::Unavailable,
                    "callback",
                    "activation preparation panicked",
                ))
            });
            if result.is_err() && host.kernel().object_address(object).is_ok() {
                host.kernel().fail(object)?;
            }
            host.flush();
            result
        })?;
        let ready = async move {
            receive
                .await
                .map_err(|_| Error::invalid(Reason::Executor, "activation preparation stopped"))?
        }
        .boxed_local()
        .shared();
        self.0.objects.borrow_mut().insert(
            object,
            Object {
                versions: BTreeMap::new(),
                activation: Some(ready),
            },
        );
        Ok(())
    }

    /// Used by a foreign language adapter after the shared kernel reserves the object.
    pub fn activate(&self, object: core::Id, orb: core::Id, service: &str) -> Result<core::Id> {
        if let Some(environment) = self
            .0
            .objects
            .borrow()
            .get(&object)
            .and_then(|object| object.versions.get(&orb))
        {
            return Ok(environment.scope);
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
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (implementation.factory)(declaration.clone())
        }))
        .unwrap_or_else(|_| {
            Err(Error::new(
                core::Code::Unavailable,
                "callback",
                "service factory panicked",
            ))
        });
        let parents = self
            .0
            .orb_scopes
            .borrow()
            .get(&orb)
            .copied()
            .into_iter()
            .collect();
        let scope = self.register_declaration(&declaration, parents)?;
        self.0
            .scope_objects
            .borrow_mut()
            .insert(scope, (object, orb));
        self.0
            .objects
            .borrow_mut()
            .entry(object)
            .or_insert_with(|| Object {
                versions: BTreeMap::new(),
                activation: None,
            })
            .versions
            .insert(
                orb,
                Environment {
                    environment: result.as_ref().cloned().unwrap_or_else(|_| Rc::new(())),
                    implementation,
                    scope,
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
