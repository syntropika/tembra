use super::*;

pub(crate) struct SetupScope;
pub(crate) struct SetupPass {
    host: Rc<HostInner>,
    _scope: Rc<SetupScope>,
    previous: Weak<SetupScope>,
}
impl SetupPass {
    pub(crate) fn enter(host: &Rc<HostInner>) -> (Self, Weak<SetupScope>) {
        let scope = Rc::new(SetupScope);
        let weak = Rc::downgrade(&scope);
        let previous = host.replace_setup_pass(weak.clone());
        (
            Self {
                host: host.clone(),
                _scope: scope,
                previous,
            },
            weak,
        )
    }
}
impl Drop for SetupPass {
    fn drop(&mut self) {
        self.host
            .replace_setup_pass(std::mem::take(&mut self.previous));
    }
}

/// Weak owned service-declaration capability for one exact synchronous facet setup.
///
/// Keeping this handle does not retain the Host, journal or pass. A setup return, failure
/// or unwind expires it; the separate FacetScope remains usable for managed cleanup roots.
#[derive(Clone)]
pub struct FacetRecorder {
    host: Weak<HostInner>,
    journal: Weak<RefCell<SetupJournal>>,
    pass: Weak<SetupScope>,
}
impl FacetDeclaration<'_> {
    /// Returns an owned weak recorder that forwards immediate service declarations.
    pub fn recorder(&self) -> FacetRecorder {
        FacetRecorder {
            host: Rc::downgrade(self.host),
            journal: Rc::downgrade(self.journal),
            pass: self.pass.clone(),
        }
    }
}
impl FacetRecorder {
    /// Retains binding state for the actual facet run selected by this exact setup.
    ///
    /// Setup failure, replacement/removal or Host close releases this anchor. Independently
    /// retained callbacks and resources may keep the value alive afterward; releasing an anchor
    /// does not prove physical callback termination. The value's own captures remain the
    /// caller's responsibility; this operation adds no independent Host or Driver task owner.
    ///
    /// # Errors
    /// Returns `invalid/setup_scope` outside the exact live setup or for a retired run, and
    /// `unavailable/closed` if the Host is unavailable. Refusal stores nothing. Capture Drop
    /// effects can occur on refusal, outside journal and run storage borrows.
    ///
    /// ```
    /// use std::{cell::RefCell, rc::Rc};
    /// use serde_json::json;
    /// use tembra_core::{ErrorCode, Host, HostOptions, LoadOptions, ManualDriver, NativeFacet};
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("example", json!(null)), driver.clone());
    /// let value = Rc::new(String::from("binding"));
    /// let weak = Rc::downgrade(&value);
    /// let pending = RefCell::new(Some(value));
    /// let saved = Rc::new(RefCell::new(None));
    /// let recorder = saved.clone();
    /// let facet = NativeFacet::new("observer", vec![], move |_, declaration| {
    ///     let current = declaration.recorder();
    ///     current.retain_run_owner(pending.borrow_mut().take().unwrap())?;
    ///     *recorder.borrow_mut() = Some(current);
    ///     Ok(())
    /// })?;
    /// driver.run_until(host.install(facet, LoadOptions::default())?)?;
    /// let error = saved.borrow().as_ref().unwrap().retain_run_owner(Rc::new(())).unwrap_err();
    /// assert_eq!((error.code, error.reason.as_str()), (ErrorCode::Invalid, "setup_scope"));
    /// assert!(weak.upgrade().is_some());
    /// driver.run_until(host.close())?;
    /// assert!(weak.upgrade().is_none());
    /// # Ok::<(), tembra_core::RuntimeError>(())
    /// ```
    pub fn retain_run_owner<T: 'static>(&self, owner: Rc<T>) -> Result<()> {
        let journal = self.journal.upgrade().ok_or_else(scope_error)?;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host no longer exists"))?;
        host.retain_run_owner(&journal, &self.pass, owner)
    }

    /// Declares through the same Host journal without retaining a borrowed declaration.
    /// No factory is started during the enclosing declaration batch.
    ///
    /// # Errors
    /// Returns `invalid/setup_scope` outside its exact pass, `unavailable/closed` for an
    /// unavailable Host, or the ordinary Host service admission failure. Refusal adds no
    /// service for this call. Prior setup effects follow the existing withdrawal policy.
    pub fn serve(
        &self,
        contract: Rc<Contract>,
        factory: Factory,
        options: ServeOptions,
    ) -> Result<()> {
        let journal = self.journal.upgrade().ok_or_else(scope_error)?;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "Host no longer exists"))?;
        host.serve_owned(&journal, &self.pass, contract, factory, options)
            .map(|_| ())
    }
}
pub(crate) fn scope_error() -> RuntimeError {
    RuntimeError::invalid("setup_scope", "recorder is outside its exact setup pass")
}
