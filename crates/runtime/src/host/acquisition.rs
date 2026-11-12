//! Reference acquisition and policy checks share the kernel's ownership graph.
use super::*;

/// Requirements applied when acquiring a service reference.
#[derive(Clone, Debug, Default)]
pub struct AcquisitionOptions {
    pub api_version: core::Version,
    pub methods: BTreeSet<String>,
}

impl Host {
    /// Acquire a reference with an explicit API version and allowed method requirement.
    pub fn get_with_options(
        &self,
        service: &str,
        key: Vec<String>,
        principal: &str,
        options: AcquisitionOptions,
    ) -> Result<Reference> {
        self.acquire(service, key, principal, options, None)
    }

    /// Acquire after the exact previous instance has physically closed, repeating admission.
    /// Other refusals return immediately. Dropping this future never cancels the old cleanup.
    /// This waits for address availability; use [`Reference::ready`] to await activation.
    pub async fn get_wait(
        &self,
        service: &str,
        key: Vec<String>,
        principal: &str,
        options: AcquisitionOptions,
    ) -> Result<Reference> {
        loop {
            match self.get_with_options(service, key.clone(), principal, options.clone()) {
                Ok(reference) => return Ok(reference),
                Err(error) => self.closing(service, &key, principal, error)?.await,
            }
        }
    }

    pub(super) fn closing(
        &self,
        service: &str,
        key: &[String],
        principal: &str,
        error: Error,
    ) -> Result<core::Disposal> {
        if error.code == core::Code::Unavailable && error.reason == Reason::Disposing.as_str() {
            let address = core::Address {
                service: service.into(),
                key: key.to_vec(),
                principal: principal.into(),
            };
            if let Some(wait) = self.kernel().wait_for_disposal(&address) {
                return Ok(wait);
            }
        }
        Err(error)
    }

    pub(super) fn acquire(
        &self,
        service: &str,
        key: Vec<String>,
        principal: &str,
        options: AcquisitionOptions,
        owner: Option<core::Owner>,
    ) -> Result<Reference> {
        if !self.0.external_actions.get() {
            self.tick();
        }
        let acquired = self.kernel().acquire(
            core::Address {
                service: service.into(),
                key,
                principal: principal.into(),
            },
            options.api_version,
            options.methods.clone(),
        )?;
        let mut foreign_ready = None;
        let result = (|| {
            if let Some(core::Owner::Object(object)) = owner {
                self.kernel().dependency(object, acquired.reference)?;
            }
            #[cfg(feature = "views")]
            {
                let access = self
                    .0
                    .orbs
                    .borrow()
                    .get(&acquired.orb)
                    .and_then(|services| services.get(service))
                    .and_then(|implementation| implementation.access.clone());
                if let Some(access) = access {
                    self.kernel()
                        .restrict(acquired.reference, access(principal))?;
                }
            }
            if !self.has_orb(acquired.orb) {
                let host = self.clone();
                let object = acquired.object;
                let address = self.kernel().object_address(object)?;
                let request = serde_json::json!({
                    "kind":"activate", "acquired":{
                        "reference":acquired.reference,"object":object,"orb":acquired.orb,
                        "codeVersion":acquired.code_version,"activate":acquired.activate,
                    },
                    "methods":options.methods,
                    "address":{"service":address.service,"key":address.key,"principal":address.principal},
                });
                let receive = self.spawn(async move {
                    let result = host.foreign(request).await.map(|_| ());
                    host.flush();
                    result
                })?;
                foreign_ready = Some(
                    async move {
                        receive.await.map_err(|_| {
                            Error::invalid(Reason::Executor, "foreign activation stopped")
                        })?
                    }
                    .boxed_local()
                    .shared(),
                );
            } else if acquired.activate {
                self.ensure_orb(acquired.orb)?;
                let before = self
                    .0
                    .orbs
                    .borrow()
                    .get(&acquired.orb)
                    .and_then(|services| services.get(service))
                    .and_then(|implementation| implementation.before_create.clone());
                if let Some(before) = before {
                    self.activate_deferred(&acquired, service.to_owned(), before)?;
                    return Ok(());
                }
                let scope = self.activate(acquired.object, acquired.orb, service)?;
                let program = self.kernel().activate_scope(scope)?;
                let activation = self.run(program)?;
                self.0
                    .objects
                    .borrow_mut()
                    .get_mut(&acquired.object)
                    .expect("activated object")
                    .activation = Some(activation);
            }
            Ok(())
        })();
        if let Err(error) = result {
            if acquired.activate {
                self.kernel().fail(acquired.object)?;
            }
            if let Some(core::Owner::Object(object)) = owner {
                self.kernel()
                    .detach_dependency(object, acquired.reference)?;
            }
            self.kernel().release(acquired.reference)?;
            self.kernel().forget_reference(acquired.reference)?;
            self.flush();
            return Err(error);
        }
        Ok(Reference {
            host: self.clone(),
            id: acquired.reference,
            object: acquired.object,
            foreign_ready,
            released: Rc::new(crate::readable::ReadLifetime::default()),
        })
    }
}

impl Host {
    /// Resolve an optional native service access policy for a shared adapter's acquisition.
    pub fn access(
        &self,
        orb: core::Id,
        service: &str,
        principal: &str,
    ) -> Option<BTreeSet<String>> {
        #[cfg(feature = "views")]
        let access = self
            .0
            .orbs
            .borrow()
            .get(&orb)
            .and_then(|services| services.get(service))
            .and_then(|implementation| implementation.access.clone());
        #[cfg(feature = "views")]
        {
            access.map(|access| access(principal))
        }
        #[cfg(not(feature = "views"))]
        {
            let _ = (orb, service, principal);
            None
        }
    }
}
