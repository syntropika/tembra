//! Native declaration storage follows kernel scopes and their dependency ownership.
use super::*;

type TaskStart = Box<dyn FnOnce(crate::TaskScope) -> LocalBoxFuture<'static, Result<()>>>;

#[derive(Default)]
pub(super) struct Draft {
    pub sealed: Cell<bool>,
    #[cfg(feature = "subscriptions")]
    pub listeners: RefCell<Vec<super::events::ListenerDefinition>>,
    #[cfg(feature = "subscriptions")]
    pub listener_names: RefCell<BTreeSet<String>>,
    pub resources: RefCell<Vec<core::Id>>,
    pub resource_names: RefCell<BTreeSet<String>>,
    pub dependencies: RefCell<Vec<Rc<Reference>>>,
    pub tasks: RefCell<Vec<TaskStart>>,
}

pub(super) struct NativeScope {
    pub owner: core::Owner,
    pub orb: core::Id,
    pub draft: Rc<Draft>,
    pub tasks: Vec<core::Id>,
}

impl Host {
    pub(super) fn declaration(&self, owner: core::Owner, orb: core::Id) -> Declaration {
        Declaration {
            host: self.clone(),
            owner,
            orb,
            object: match owner {
                core::Owner::Object(object) => object,
                _ => 0,
            },
            draft: Rc::new(Draft::default()),
        }
    }

    pub(super) fn register_declaration(
        &self,
        declaration: &Declaration,
        parents: Vec<core::Id>,
    ) -> Result<core::Id> {
        let scope = self.kernel().allocate_handle()?;
        self.kernel().register_scope(core::Scope {
            id: scope,
            owner: declaration.owner,
            orb: declaration.orb,
            parents,
            dependencies: declaration
                .draft
                .dependencies
                .borrow()
                .iter()
                .map(|reference| reference.id())
                .collect(),
            resources: declaration.draft.resources.borrow().clone(),
            state_version: None,
            migrate: false,
            active: false,
            closing: false,
        })?;
        declaration.draft.sealed.set(true);
        self.0.scopes.borrow_mut().insert(
            scope,
            NativeScope {
                owner: declaration.owner,
                orb: declaration.orb,
                draft: declaration.draft.clone(),
                tasks: Vec::new(),
            },
        );
        Ok(scope)
    }

    /// Declare a host-owned environment and resources before installing or acquiring orbs.
    /// The kernel activates the scope and closes it after all child scopes settle.
    pub fn declare<T>(&self, create: impl FnOnce(Declaration) -> Result<T>) -> Result<T> {
        if !self.kernel().installed_orbs().is_empty() {
            return Err(Error::invalid(
                Reason::Scope,
                "declare host resources before installing orbs",
            ));
        }
        let declaration = self.declaration(core::Owner::Host, 0);
        let result = create(declaration.clone());
        let scope = self.register_declaration(&declaration, Vec::new())?;
        if result.is_ok() {
            self.0.host_scopes.borrow_mut().push(scope);
        }
        let program = if result.is_ok() {
            self.kernel().activate_scope(scope)?
        } else {
            self.kernel().cleanup_scopes(vec![scope], None)?
        };
        let ready = self.run(program)?;
        if result.is_ok() {
            self.0.scope_activations.borrow_mut().insert(scope, ready);
        }
        result
    }

    /// Read a named host resource. Its value remains unavailable until activation completes.
    pub fn resource<T: 'static>(&self, name: &str) -> Result<Resource<T>> {
        let key = self
            .0
            .host_resources
            .borrow()
            .get(name)
            .cloned()
            .ok_or_else(|| Error::invalid(Reason::Resource, name))?;
        Ok(Resource {
            key,
            marker: std::marker::PhantomData,
        })
    }

    pub(super) fn ensure_orb(&self, orb: core::Id) -> Result<()> {
        if let Some(error) = self.0.orb_errors.borrow().get(&orb).cloned() {
            return Err(error);
        }
        if self.0.orb_scopes.borrow().contains_key(&orb) {
            return Ok(());
        }
        if !self.0.orb_declaring.borrow_mut().insert(orb) {
            return Err(Error::invalid(
                Reason::DependencyCycle,
                "orb declaration dependency cycle",
            ));
        }
        let result = self.declare_orb(orb);
        self.0.orb_declaring.borrow_mut().remove(&orb);
        result
    }

    fn declare_orb(&self, orb: core::Id) -> Result<()> {
        let factory = self
            .0
            .orb_factories
            .borrow()
            .get(&orb)
            .cloned()
            .ok_or_else(|| Error::invalid(Reason::Orb, "orb factory is unavailable"))?;
        let info = self.kernel().orb(orb)?;
        let declaration = self.declaration(core::Owner::Orb(info.owner), orb);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            factory(declaration.clone())
        }))
        .unwrap_or_else(|_| {
            Err(Error::new(
                core::Code::Unavailable,
                "callback",
                "orb factory panicked",
            ))
        });
        if let Err(error) = &result {
            self.0.orb_errors.borrow_mut().insert(orb, error.clone());
        }
        let scope = self.register_declaration(&declaration, self.0.host_scopes.borrow().clone())?;
        self.0.orb_scopes.borrow_mut().insert(orb, scope);
        self.0.orb_values.borrow_mut().insert(
            orb,
            result.as_ref().cloned().unwrap_or_else(|_| Rc::new(())),
        );
        let program = if result.is_ok() {
            self.kernel().activate_scope(scope)?
        } else {
            self.kernel().cleanup_scopes(vec![scope], None)?
        };
        let ready = self.run(program)?;
        if result.is_ok() {
            self.0.scope_activations.borrow_mut().insert(scope, ready);
        }
        result.map(|_| ())
    }
}

impl Declaration {
    /// Access the shared environment created once by this orb's factory.
    pub fn orb<T: 'static>(&self) -> Result<Rc<T>> {
        self.host
            .0
            .orb_values
            .borrow()
            .get(&self.orb)
            .cloned()
            .ok_or_else(|| Error::invalid(Reason::Orb, "orb environment is not declared"))?
            .downcast::<T>()
            .map_err(|_| type_error())
    }

    /// Resolve a host resource by its declaration name.
    pub fn host_resource<T: 'static>(&self, name: &str) -> Result<Resource<T>> {
        self.host.resource(name)
    }

    /// Acquire a required service with this scope's ownership and readiness edges.
    /// Factories receive the handle immediately; activation waits for its dependency.
    pub fn get(&self, service: &str, key: Vec<String>) -> Result<Rc<Reference>> {
        self.check_declaration()?;
        if self.orb == 0 {
            return Err(Error::invalid(
                Reason::Dependency,
                "host declarations have no orb requirements",
            ));
        }
        let info = self.host.kernel().orb(self.orb)?;
        let options = if info
            .declaration
            .services
            .iter()
            .any(|own| own.name == service)
        {
            crate::AcquisitionOptions {
                api_version: info.declaration.api_version,
                ..Default::default()
            }
        } else {
            let requirement = info
                .declaration
                .requires
                .iter()
                .find(|requirement| requirement.methods.contains_key(service))
                .ok_or_else(|| {
                    Error::new(core::Code::Forbidden, Reason::DependencyService, service)
                })?;
            crate::AcquisitionOptions {
                api_version: requirement.api_version,
                methods: requirement.methods[service].clone(),
            }
        };
        let principal = if self.object == 0 {
            "host".into()
        } else {
            self.address()?.principal
        };
        // Register the dependency edge before activating the provider so cycles fail synchronously.
        let reference =
            Rc::new(
                self.host
                    .acquire(service, key, &principal, options, Some(self.owner))?,
            );
        self.draft.dependencies.borrow_mut().push(reference.clone());
        Ok(reference)
    }
}

impl Host {
    /// Wait for the currently declared host and orb scopes, including asynchronous openers.
    pub async fn ready(&self) -> Result<()> {
        if let Some(error) = self.0.orb_errors.borrow().values().next().cloned() {
            return Err(error);
        }
        let activations = self
            .0
            .scope_activations
            .borrow()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for activation in activations {
            activation.await?;
        }
        Ok(())
    }

    #[cfg(feature = "hooks")]
    pub(crate) async fn ready_orb(&self, orb: core::Id) -> Result<()> {
        if let Some(error) = self.0.orb_errors.borrow().get(&orb).cloned() {
            return Err(error);
        }
        let scope = self.0.orb_scopes.borrow().get(&orb).copied();
        let ready = scope.and_then(|scope| self.0.scope_activations.borrow().get(&scope).cloned());
        if let Some(ready) = ready {
            ready.await?;
        }
        Ok(())
    }
}
