use super::*;
use crate::{GetOptions, InvocationOwner, Registry};

pub(crate) struct FactoryScope {
    pub(crate) target: crate::host::reload::DeclarationTarget,
}

/// An owned declaration capability valid only during its exact synchronous factory pass.
///
/// Cloning this recorder does not retain its Host, object or pass. Operations forward to
/// the actual core Declaration and return actual handles; there is no delayed replay.
/// Retained or wrong-pass use refuses before changing declarations. Successfully declared
/// values and external callback effects are not rolled back by a later factory error.
///
/// ```
/// use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
/// use serde_json::json;
/// use tembra_core::{contracts::Contract, *};
/// let driver = ManualDriver::new();
/// let host = Host::new(HostOptions::new("example", json!(null)), driver.clone());
/// let token = Rc::new(Contract::local("recorded", 1, json!({}), BTreeMap::new())?);
/// let saved = Rc::new(RefCell::new(None));
/// let captured = saved.clone();
/// host.serve(token.clone(), Rc::new(move |_, _, declaration| {
///     let recorder = declaration.recorder();
///     let cell = recorder.state("count", StateDeclaration::new(json!({}), json!(3)))?;
///     *captured.borrow_mut() = Some((recorder, cell));
///     Ok(Implementation::new())
/// }), ServeOptions::default())?;
/// let reference = driver.run_until(host.get(token, json!({}), GetOptions::default()))?;
/// let (recorder, cell) = saved.borrow().clone().unwrap();
/// let error = recorder.state("late", StateDeclaration::new(json!({}), json!(9))).err().unwrap();
/// assert_eq!(error.reason, "declaration_scope");
/// assert_eq!(cell.snapshot()?.value(), &json!(3));
/// reference.release();
/// driver.run_until(host.close())?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct DeclarationRecorder {
    host: Weak<HostInner>,
    object: Weak<ObjectRecord>,
    pass: Weak<FactoryScope>,
}

impl Declaration {
    pub(crate) fn reload_candidate(
        &self,
    ) -> Result<Option<Rc<crate::host::reload::CandidateDeclarations>>> {
        let pass = self.pass.upgrade().ok_or_else(scope_error)?;
        let exact = Weak::ptr_eq(&self.host.factory_pass.borrow(), &Rc::downgrade(&pass));
        if !exact {
            return Err(scope_error());
        }
        match &pass.target {
            crate::host::reload::DeclarationTarget::Activation => Ok(None),
            crate::host::reload::DeclarationTarget::Reload(candidate) => candidate
                .upgrade()
                .filter(|candidate| Weak::ptr_eq(&candidate.owner, &Rc::downgrade(&self.object)))
                .map(Some)
                .ok_or_else(scope_error),
        }
    }

    pub(crate) fn activity_declarations(
        &self,
    ) -> Result<Rc<crate::activity::ActivityDeclarations>> {
        Ok(match self.reload_candidate()? {
            Some(candidate) => candidate.activities.clone(),
            None => self.object.activities.clone(),
        })
    }

    /// Returns a weak owned recorder for this exact factory pass.
    pub fn recorder(&self) -> DeclarationRecorder {
        DeclarationRecorder {
            host: Rc::downgrade(&self.host),
            object: Rc::downgrade(&self.object),
            pass: self.pass.clone(),
        }
    }

    /// Mints weak ownership for later physical retention of entered receiver calls.
    /// The owner alone neither keeps resources alive nor authorizes an invocation.
    pub fn invocation_owner(&self) -> InvocationOwner {
        InvocationOwner::new(&self.host, &self.object)
    }
}

impl DeclarationRecorder {
    pub(crate) fn declaration(&self) -> Result<Declaration> {
        let scope = self.pass.upgrade().ok_or_else(scope_error)?;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host no longer exists"))?;
        if !Weak::ptr_eq(&host.factory_pass.borrow(), &Rc::downgrade(&scope)) {
            return Err(scope_error());
        }
        let object = self
            .object
            .upgrade()
            .filter(|object| {
                object.phase.get() == Phase::Activating
                    || (matches!(
                        scope.target,
                        crate::host::reload::DeclarationTarget::Reload(_)
                    ) && object.phase.get() == Phase::Live)
            })
            .ok_or_else(|| {
                RuntimeError::new(
                    crate::ErrorCode::Gone,
                    "declaration_owner",
                    "declaration owner ended",
                )
            })?;
        Ok(Declaration {
            host,
            object,
            pass: self.pass.clone(),
        })
    }

    /// Returns this receiver's weak invocation owner during the factory pass.
    ///
    /// # Errors
    /// Refuses expired/wrong-pass use with `invalid/declaration_scope`, unavailable Host
    /// with `unavailable/closed`, or ended owner with `gone/declaration_owner`.
    pub fn invocation_owner(&self) -> Result<InvocationOwner> {
        Ok(self.declaration()?.invocation_owner())
    }

    /// Declares an actual state cell, including immediate schema/value checks.
    ///
    /// # Errors
    /// Returns recorder scope/owner failures or Declaration::state errors unchanged.
    pub fn state(
        &self,
        name: impl Into<String>,
        declaration: StateDeclaration,
    ) -> Result<crate::Cell> {
        self.declaration()?.state(name, declaration)
    }

    /// Declares an actual synchronous projection without replaying its declaration.
    ///
    /// # Errors
    /// Returns recorder scope/owner failures or Declaration::derive errors unchanged.
    pub fn derive(
        &self,
        sources: Vec<Readable>,
        schema: Value,
        compute: impl Fn(&[Snapshot]) -> Result<Value> + 'static,
    ) -> Result<Readable> {
        self.declaration()?.derive(sources, schema, compute)
    }

    /// Acquires the actual object-owned dependency through existing cycle checks.
    ///
    /// # Errors
    /// Returns recorder scope/owner failures or Declaration::get errors unchanged.
    pub fn get(
        &self,
        contract: Rc<Contract>,
        input: Value,
        options: GetOptions,
    ) -> Result<Reference> {
        self.declaration()?.get(contract, input, options)
    }

    /// Records a resource without opening it during the factory.
    ///
    /// # Errors
    /// Returns recorder scope/owner failures or Declaration::resource errors unchanged.
    pub fn resource<T: 'static>(
        &self,
        name: impl Into<String>,
        declaration: ResourceDeclaration<T>,
    ) -> Result<Resource<T>> {
        self.declaration()?.resource(name, declaration)
    }

    /// Adds checked metadata through the same registry declaration path.
    ///
    /// # Errors
    /// Returns recorder scope/owner failures or Declaration::register errors unchanged.
    pub fn register(&self, registry: &Registry, key: impl Into<String>, data: Value) -> Result<()> {
        self.declaration()?.register(registry, key, data)
    }

    /// Adds metadata naming the receiver's checked contract method.
    ///
    /// # Errors
    /// Returns recorder scope/owner failures or Declaration::register_method errors unchanged.
    pub fn register_method(
        &self,
        registry: &Registry,
        key: impl Into<String>,
        data: Value,
        member: impl Into<String>,
    ) -> Result<()> {
        self.declaration()?
            .register_method(registry, key, data, member)
    }
}

fn scope_error() -> RuntimeError {
    RuntimeError::invalid(
        "declaration_scope",
        "recorder is outside its exact factory pass",
    )
}
