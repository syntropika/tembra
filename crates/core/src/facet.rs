//! Synchronous facet declarations and their asynchronous startup waiter.
//!
//! The Host owns installed services, activation and cleanup. Dropping [`FacetStartup`] only
//! abandons a readiness waiter. A retained [`FacetScope`] has weak Host ownership; its signal
//! notifies run ownership changes without preventing managed cleanup roots.

mod recorder;
use crate::host::{HostInner, facets::SetupJournal};
use crate::{
    CancellationToken, Context, Factory, Host, Registry, RegistryView, Result, RuntimeError,
    ServeOptions,
};
use futures::future::{FutureExt, LocalBoxFuture};
pub use recorder::FacetRecorder;
pub(crate) use recorder::{SetupPass, SetupScope};
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context as TaskContext, Poll},
};
use tembra_contracts::Contract;

type Setup = dyn Fn(&FacetScope, &mut FacetDeclaration<'_>) -> Result<()>;

struct Definition {
    name: String,
    uses: Vec<Rc<Contract>>,
    setup: Box<Setup>,
}

/// An immutable executable native facet with a stable name and synchronous setup.
///
/// Clones share the definition and setup callback. Consumed contract tokens are immutable
/// compatibility metadata, not implicit acquisitions or authority. Native definitions do not
/// hot-reload; duplicate installation is refused by the Host.
#[derive(Clone)]
pub struct NativeFacet {
    definition: Rc<Definition>,
}

impl NativeFacet {
    /// Validates a stable name and stores the owned token list and setup callback.
    ///
    /// Name spelling is preserved exactly, including whitespace. Constructing a definition
    /// neither invokes setup nor acquires objects. Tokens can share immutable contract storage;
    /// changing the caller's original vector cannot change this definition's list.
    ///
    /// # Errors
    /// Returns `invalid/facet_name` for an empty name or a name containing a control character.
    /// No callback or Host mutation occurs on refusal.
    ///
    /// ```
    /// use std::{cell::Cell, rc::Rc};
    /// use tembra_core::{Host, HostOptions, LoadOptions, ManualDriver, NativeFacet};
    /// use serde_json::json;
    ///
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("example", json!(null)), driver.clone());
    /// let calls = Rc::new(Cell::new(0));
    /// let observed = calls.clone();
    /// let facet = NativeFacet::new("example", vec![], move |_, _| {
    ///     observed.set(observed.get() + 1);
    ///     Ok(())
    /// })?;
    /// assert_eq!(calls.get(), 0);
    /// assert!(NativeFacet::new("", vec![], |_, _| Ok(())).is_err());
    /// let startup = host.install(facet.clone(), LoadOptions::default())?;
    /// assert_eq!(calls.get(), 1); // Setup ran before the waiter was polled.
    /// assert!(host.install(facet, LoadOptions::default()).is_err());
    /// assert_eq!(calls.get(), 1); // Duplicate refusal did not run setup.
    /// driver.run_until(startup)?;
    /// driver.run_until(host.close())?;
    /// # Ok::<(), tembra_core::RuntimeError>(())
    /// ```
    pub fn new(
        name: impl Into<String>,
        uses: Vec<Rc<Contract>>,
        setup: impl Fn(&FacetScope, &mut FacetDeclaration<'_>) -> Result<()> + 'static,
    ) -> Result<Self> {
        let name = name.into();
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(RuntimeError::invalid(
                "facet_name",
                "facet name must be nonempty and contain no control characters",
            ));
        }
        Ok(Self {
            definition: Rc::new(Definition {
                name,
                uses,
                setup: Box::new(setup),
            }),
        })
    }

    /// Returns the exact stable facet name.
    pub fn name(&self) -> &str {
        &self.definition.name
    }

    /// Returns the consumed tokens in their original order, including distinct same-name tokens.
    pub fn uses(&self) -> &[Rc<Contract>] {
        &self.definition.uses
    }
}

/// Lifecycle origin of an executable definition. Only foreign definitions can be replaced;
/// this tag is not an engine handle, permission grant or execution-version identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FacetKind {
    Native,
    Foreign,
}

/// An owned observation of one run in the Host's installed facet catalog.
///
/// Presence establishes catalog admission, not readiness, startup success or package provenance.
/// Absence does not establish that old calls stopped or resources were physically released.
/// This value retains no executable callbacks or resources and carries no mutation authority.
/// Equality compares only name and origin; same-name replacement can produce an equal value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledFacet {
    name: String,
    kind: FacetKind,
}

impl InstalledFacet {
    pub(crate) fn from_definition(definition: &ExecutableFacet) -> Self {
        Self {
            name: definition.name().to_owned(),
            kind: definition.kind(),
        }
    }

    /// Returns the exact executable name, not a generation or code-version identity.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the definition's native or foreign origin, independent of readiness.
    pub fn kind(&self) -> FacetKind {
        self.kind
    }
}

/// Engine-neutral executable definition using the Host's existing synchronous installer.
///
/// Native conversions preserve definition identity. Foreign callbacks own their engine
/// generation captures; the core neither serializes those handles nor runs another engine.
#[derive(Clone)]
pub struct ExecutableFacet {
    definition: Rc<Definition>,
    kind: FacetKind,
}
impl From<NativeFacet> for ExecutableFacet {
    fn from(value: NativeFacet) -> Self {
        Self {
            definition: value.definition,
            kind: FacetKind::Native,
        }
    }
}
impl ExecutableFacet {
    /// Creates a foreign definition without running setup or acquiring objects.
    /// Duplicate installation is still refused; this does not enable hot replacement.
    ///
    /// # Errors
    /// Returns `invalid/facet_name` for the same invalid names as NativeFacet::new.
    pub fn foreign(
        name: impl Into<String>,
        uses: Vec<Rc<Contract>>,
        setup: impl Fn(&FacetScope, &mut FacetDeclaration<'_>) -> Result<()> + 'static,
    ) -> Result<Self> {
        let definition = NativeFacet::new(name, uses, setup)?.definition;
        Ok(Self {
            definition,
            kind: FacetKind::Foreign,
        })
    }
    pub fn kind(&self) -> FacetKind {
        self.kind
    }
    pub fn name(&self) -> &str {
        &self.definition.name
    }
    pub fn uses(&self) -> &[Rc<Contract>] {
        &self.definition.uses
    }

    pub(crate) fn setup(
        &self,
        scope: &FacetScope,
        declaration: &mut FacetDeclaration<'_>,
    ) -> Result<()> {
        (self.definition.setup)(scope, declaration)
    }
}

/// A retainable, weak Host API for a facet's roots, registries and run notification.
///
/// This does not sandbox trusted native code that independently captures a Host. It omits
/// declaration authority so retaining a scope cannot extend its setup phase.
#[derive(Clone)]
pub struct FacetScope {
    host: Weak<HostInner>,
    host_id: u64,
    host_name: Rc<str>,
    facet_name: Rc<str>,
    signal: FacetSignal,
}

impl FacetScope {
    pub(crate) fn same_host(&self, host: &Rc<HostInner>) -> bool {
        Weak::ptr_eq(&self.host, &Rc::downgrade(host))
    }

    pub(crate) fn new(host: &Rc<HostInner>, facet_name: Rc<str>, signal: FacetSignal) -> Self {
        Self {
            host: Rc::downgrade(host),
            host_id: host.id,
            host_name: Rc::from(host.name.as_str()),
            facet_name,
            signal,
        }
    }

    /// Returns the Host name, which is independent of the facet's name.
    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    /// Returns the originating Host identity as observation, without retaining or authorizing it.
    pub fn host_id(&self) -> u64 {
        self.host_id
    }

    /// Returns read-only run cancellation notification.
    ///
    /// Cancellation does not prove physical callback completion or prohibit cleanup roots.
    pub fn signal(&self) -> FacetSignal {
        self.signal.clone()
    }

    /// Runs a root through the Host's existing current-version admission path.
    /// Calls made with this root Context attribute this scope's installed Facet as their caller.
    ///
    /// The root scope expires when its future settles or is dropped. The run signal does not
    /// gate this operation: a resource closer may still use a retained scope and held dependency
    /// during managed Host cleanup. Full Host closure refuses new roots. The returned future
    /// holds no strong Host owner until polled; an executing root retains its normal owner.
    ///
    /// # Errors
    /// Returns `unavailable/closed` if the Host has been dropped or fully closed, or the callback
    /// error unchanged. Callback errors can follow external effects; cancellation is cooperative.
    ///
    /// # Panics
    /// A native callback panic propagates according to the build's panic strategy.
    pub fn root<T: 'static>(
        &self,
        callback: impl FnOnce(Context) -> LocalBoxFuture<'static, Result<T>> + 'static,
    ) -> LocalBoxFuture<'static, Result<T>> {
        let host = self.host.clone();
        let facet_name = self.facet_name.clone();
        async move {
            let host = host
                .upgrade()
                .ok_or_else(|| RuntimeError::unavailable("closed", "host is closed"))?;
            Host { inner: host }
                .root(move |context| callback(context.with_facet_root(facet_name)))
                .await
        }
        .boxed_local()
    }

    /// Obtains the Host's checked registry view without acquiring contribution objects.
    ///
    /// # Errors
    /// Returns `unavailable/closed` if the Host is no longer available, or the Host registry
    /// admission error unchanged, including an inconsistent same-name definition.
    pub fn registry(&self, registry: &Registry) -> Result<RegistryView> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "host is closed"))?;
        Host { inner: host }.registry(registry)
    }
}

/// A borrowed, setup-only service declaration capability.
///
/// The Host journal owns admission and withdrawal. A setup error withdraws only the failing
/// run's scoped declarations and unstarted tickets; prior successful facets and external
/// effects remain. Retain [`FacetScope`] for later roots or registry access instead.
///
/// A declaration cannot be stored as a static serve capability:
///
/// ```compile_fail
/// use std::{cell::RefCell, rc::Rc};
/// use tembra_core::{FacetDeclaration, NativeFacet};
/// let retained = Rc::new(RefCell::new(None::<&'static mut FacetDeclaration<'static>>));
/// NativeFacet::new("example", vec![], move |_, declaration| {
///     *retained.borrow_mut() = Some(declaration);
///     Ok(())
/// }).unwrap();
/// ```
pub struct FacetDeclaration<'setup> {
    host: &'setup Rc<HostInner>,
    journal: &'setup Rc<RefCell<SetupJournal>>,
    pass: Weak<SetupScope>,
}

impl<'setup> FacetDeclaration<'setup> {
    pub(crate) fn new(
        host: &'setup Rc<HostInner>,
        journal: &'setup Rc<RefCell<SetupJournal>>,
        pass: Weak<SetupScope>,
    ) -> Self {
        Self {
            host,
            journal,
            pass,
        }
    }

    /// Prepares immutable definition metadata in this exact setup and retains it in its run.
    /// # Errors
    /// Rejects expired or foreign setup before the producer and rechecks after it returns.
    /// Producer errors retain nothing; independent callback effects are not rolled back.
    pub fn prepare_metadata<T: 'static>(
        &self,
        prepare: impl FnOnce() -> Result<T>,
    ) -> Result<Rc<T>> {
        self.host.check_setup(self.journal, &self.pass)?;
        let value = Rc::new(prepare()?);
        self.host
            .retain_run_owner(self.journal, &self.pass, value.clone())?;
        Ok(value)
    }

    /// Declares a service through the Host's scoped installation journal.
    ///
    /// Host-lifetime activation is scheduled after the outer declaration batch finishes;
    /// referenced services remain lazy. This method does not run a factory or wait for readiness.
    ///
    /// # Errors
    /// Returns the Host's declaration error unchanged, including duplicate services, closed Host,
    /// invalid placement or host-lifetime options. Refusal adds no service for this call; prior
    /// successful declarations are withdrawn only if the surrounding setup fails.
    pub fn serve(
        &mut self,
        contract: Rc<Contract>,
        factory: Factory,
        options: ServeOptions,
    ) -> Result<()> {
        self.host
            .serve_owned(self.journal, &self.pass, contract, factory, options)
            .map(|_| ())
    }
}

/// Read-only notification for the lifetime of one facet run.
///
/// The Host owns cancellation authority. Notification does not revoke ordinary Host roots or
/// prove termination of work already running. Foreign replacement stops the replaced run;
/// native facets remain stable and cannot be replaced through reload.
///
/// A signal cannot be used to cancel its run:
///
/// ```compile_fail
/// use tembra_core::{FacetSignal, RuntimeError};
/// fn cancel_run(signal: FacetSignal) {
///     signal.cancel(RuntimeError::aborted("external", "not owner authority"));
/// }
/// ```
#[derive(Clone)]
pub struct FacetSignal {
    cancellation: CancellationToken,
}

impl FacetSignal {
    pub(crate) fn new(cancellation: CancellationToken) -> Self {
        Self { cancellation }
    }

    /// Returns whether the run's owner has signalled cancellation.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// Waits for cancellation and returns its original reason.
    ///
    /// Dropping this waiter neither cancels the run nor stops physical work.
    pub fn cancelled(&self) -> LocalBoxFuture<'static, RuntimeError> {
        self.cancellation.cancelled()
    }
}

/// Controls whether the startup waiter includes eager host-object readiness.
#[derive(Clone, Copy, Debug)]
pub struct LoadOptions {
    /// Wait for the installed batch's host objects; false skips this entire wait.
    pub await_host_objects: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            await_host_objects: true,
        }
    }
}

/// A readiness waiter for declarations that the Host has already installed synchronously.
///
/// The Host, rather than this handle, owns installed runs, service declarations and physical
/// activation. Dropping the handle abandons only its wait. An activation error can follow
/// successful installations and external setup effects; it does not roll them back.
#[must_use = "await startup to observe readiness, or explicitly drop only the waiter"]
pub struct FacetStartup {
    future: LocalBoxFuture<'static, Result<()>>,
}

impl FacetStartup {
    pub(crate) fn new(future: LocalBoxFuture<'static, Result<()>>) -> Self {
        Self { future }
    }
}

impl Future for FacetStartup {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_observation_has_no_cancellation_authority() {
        let installed = CancellationToken::new();
        let candidate = CancellationToken::new();
        let installed_signal = FacetSignal::new(installed.clone());
        let candidate_signal = FacetSignal::new(candidate.clone());
        let mut original = RuntimeError::aborted("candidate", "candidate setup failed");
        original.set_details(serde_json::json!({"source": "setup"}));
        candidate.cancel(original.clone());
        assert!(candidate_signal.is_cancelled());
        assert!(!installed_signal.is_cancelled());
        let reason = futures::executor::block_on(candidate_signal.cancelled());
        assert_eq!(reason, original);
        installed.cancel(RuntimeError::aborted("closed", "host closed"));
        assert!(installed_signal.is_cancelled());
    }
}
