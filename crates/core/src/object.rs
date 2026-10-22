mod owner;
pub use owner::WeakImplementationOwner;

mod recorder;
pub use recorder::DeclarationRecorder;
pub(crate) use recorder::FactoryScope;

use crate::state::{Exposure, StateGraph};
use crate::{Context, Readable, Result, RuntimeError, Snapshot, StateDeclaration};
use crate::{
    host::HostInner,
    reference::{Claim, Reference},
    resource::{Resource, ResourceDeclaration, ResourceRecord},
};
use futures::future::{FutureExt, LocalBoxFuture};
use serde_json::Value;
use std::{
    any::{Any, TypeId},
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap},
    rc::{Rc, Weak},
};
use tembra_contracts::Contract;

/// Synchronous declaration callback. Resource access and reference invocation are refused
/// during this pass. External effects performed directly by the callback cannot be rolled back.
pub type Factory = Rc<dyn Fn(Value, Value, &mut Declaration) -> Result<Implementation>>;
pub(crate) type ErasedMethod =
    Rc<dyn Fn(Box<dyn Any>, Context) -> LocalBoxFuture<'static, Result<Box<dyn Any>>>>;

pub(crate) type OpaqueGetter = Rc<dyn Fn() -> Result<Rc<dyn Any>>>;
pub type JsonMethod =
    Rc<dyn Fn(Option<Value>, Context) -> LocalBoxFuture<'static, Result<Option<Value>>>>;
pub type ContractAdapter = Rc<dyn Fn(Rc<Implementation>) -> Result<Implementation>>;

#[derive(Clone, Default)]
pub struct Implementation {
    pub(crate) methods: HashMap<String, NativeMethod>,
    pub(crate) json_methods: HashMap<String, JsonMethod>,
    pub(crate) opaque: HashMap<String, OpaqueGetter>,
    pub(crate) states: HashMap<String, Readable>,
    pub(crate) variant: Option<String>,
    // Keep owners through callback/handle capture destruction as well as invocation.
    owners: Vec<Rc<dyn Any>>,
}

#[derive(Clone)]
pub(crate) struct NativeMethod {
    pub invoke: ErasedMethod,
    pub args: TypeId,
    pub result: TypeId,
}

impl Implementation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retains callback/generation ownership without adding a contract member.
    ///
    /// Cloning preserves these owners; adapters must retain the source captures they use.
    /// Entered invocations retain their
    /// selected implementation, including these captures, until physical leases release.
    /// Ordinary disposal releases implementation owners after resources close and declared
    /// dependencies release, and after its callback captures are destroyed. A failed
    /// candidate drops its captures; this is not an async
    /// cleanup hook or a guarantee that native destructor effects can be rolled back.
    ///
    /// ```
    /// use std::rc::Rc;
    /// use tembra_core::Implementation;
    /// let owner = Rc::new(());
    /// let weak = Rc::downgrade(&owner);
    /// let candidate = Implementation::new().retain_owner(owner);
    /// let selected = candidate.clone();
    /// drop(candidate);
    /// assert!(weak.upgrade().is_some());
    /// drop(selected);
    /// assert!(weak.upgrade().is_none());
    /// ```
    pub fn retain_owner<T: 'static>(mut self, owner: Rc<T>) -> Self {
        self.owners.push(owner);
        self
    }

    fn retains_owner<T: 'static>(&self, value: &Rc<T>) -> bool {
        let erased: Rc<dyn Any> = value.clone();
        self.owners.iter().any(|owner| Rc::ptr_eq(owner, &erased))
    }

    /// Returns the first retained capture of the requested Rust type, if one was attached.
    ///
    /// This clones its ownership handle without changing the implementation or dispatching code.
    /// Execution adapters can recover their private implementation metadata without exposing it
    /// as a contract member. The returned handle may outlive this implementation; it does not
    /// grant a Context, revive disposed objects or establish physical callback completion.
    ///
    /// ```
    /// use std::rc::Rc;
    /// use tembra_core::Implementation;
    /// let owner = Rc::new(String::from("session"));
    /// let weak = Rc::downgrade(&owner);
    /// let implementation = Implementation::new().retain_owner(owner.clone());
    /// assert!(implementation.retained_owner::<usize>().is_none());
    /// let retained = implementation.retained_owner::<String>().unwrap();
    /// assert!(Rc::ptr_eq(&owner, &retained));
    /// drop(owner);
    /// drop(implementation);
    /// assert!(weak.upgrade().is_some());
    /// drop(retained);
    /// assert!(weak.upgrade().is_none());
    /// ```
    pub fn retained_owner<T: 'static>(&self) -> Option<Rc<T>> {
        self.owners
            .iter()
            .find_map(|owner| owner.clone().downcast::<T>().ok())
    }

    /// Registers a checked native dispatch channel on this local implementation candidate.
    ///
    /// Calls preserve Rust values and check their type identities without JSON conversion.
    /// Portable schema metadata is not validated by this native channel; register a portable
    /// channel and use `Reference::call_json` when that validation is required. Remote
    /// implementations cannot register native channels. Reusing a name replaces this entry.
    pub fn method<A: 'static, R: 'static>(
        mut self,
        name: impl Into<String>,
        method: impl Fn(A, Context) -> LocalBoxFuture<'static, Result<R>> + 'static,
    ) -> Self {
        let method = Rc::new(method);
        self.methods.insert(
            name.into(),
            NativeMethod {
                args: TypeId::of::<A>(),
                result: TypeId::of::<R>(),
                invoke: Rc::new(move |args, ctx| {
                    let method = method.clone();
                    async move {
                        let args = args.downcast::<A>().map_err(|_| {
                            RuntimeError::invalid(
                                "native_type",
                                "native method argument type mismatch",
                            )
                        })?;
                        method(*args, ctx)
                            .await
                            .map(|result| Box::new(result) as Box<dyn Any>)
                    }
                    .boxed_local()
                }),
            },
        );
        self
    }

    pub fn json_method(
        mut self,
        name: impl Into<String>,
        method: impl Fn(Option<Value>, Context) -> LocalBoxFuture<'static, Result<Option<Value>>>
        + 'static,
    ) -> Self {
        self.json_methods.insert(name.into(), Rc::new(method));
        self
    }

    pub fn variant(mut self, name: impl Into<String>) -> Self {
        self.variant = Some(name.into());
        self
    }

    /// Obtain a callback to construct an explicit compatibility view.
    ///
    /// This does not acquire an object, start a root, or validate a payload. The
    /// callback is intended for provider adapters, which the reference dispatcher
    /// invokes under a managed call scope. Application callers use `Reference`.
    ///
    /// # Errors
    /// Returns `not_in_variant` if no portable method has this name.
    pub fn portable_method(&self, name: &str) -> Result<JsonMethod> {
        self.json_methods.get(name).cloned().ok_or_else(|| {
            RuntimeError::new(
                crate::ErrorCode::NotInVariant,
                "method",
                "portable method does not exist",
            )
        })
    }

    /// Copies every registered dispatch channel of `target` to `alias`.
    ///
    /// Native callbacks retain their Rust type identities; portable callbacks remain
    /// separate. Replacing an existing alias removes any channel absent from the target,
    /// so a stale callback cannot survive under the new alias. Aliasing a target to itself
    /// preserves all its channels.
    ///
    /// # Errors
    /// Returns `not_in_variant` when neither target channel exists. On failure the
    /// consumed candidate is discarded; the installed provider is never mutated.
    pub fn alias_method(mut self, alias: impl Into<String>, target: &str) -> Result<Self> {
        let alias = alias.into();
        let native = self.methods.get(target).cloned();
        let portable = self.json_methods.get(target).cloned();
        if native.is_none() && portable.is_none() {
            return Err(RuntimeError::new(
                crate::ErrorCode::NotInVariant,
                "method",
                "alias target does not exist",
            ));
        }
        if let Some(method) = native {
            self.methods.insert(alias.clone(), method);
        } else {
            self.methods.remove(&alias);
        }
        if let Some(method) = portable {
            self.json_methods.insert(alias, method);
        } else {
            self.json_methods.remove(&alias);
        }
        Ok(self)
    }

    /// Exposes one source member while retaining the complete source implementation.
    ///
    /// Both native and portable channels are copied without invoking callbacks or codecs.
    /// Missing source channels replace any prior destination channel. Other source members,
    /// state and capabilities are not exposed. The whole source remains owned until this projection
    /// and its physical invocations release it; the selected Context still controls dispatch.
    ///
    /// # Errors
    /// Returns `not_in_variant` when the source has neither channel for `target`. The source is
    /// unchanged; the consumed destination candidate is discarded on failure.
    ///
    /// ```
    /// use std::rc::Rc;
    /// use futures::FutureExt;
    /// use serde_json::json;
    /// use tembra_core::{ErrorCode, Host, HostOptions, Implementation, ManualDriver};
    /// let source = Rc::new(Implementation::new()
    ///     .json_method("current", |value, _| async move { Ok(value) }.boxed_local())
    ///     .json_method("private", |_, _| async { Ok(None) }.boxed_local()));
    /// let refused = Implementation::new().project_method("old", source.clone(), "missing")
    ///     .err().expect("missing source method must refuse");
    /// assert_eq!(refused.code, ErrorCode::NotInVariant);
    /// let view = Implementation::new().project_method("old", source, "current")?;
    /// assert!(view.portable_method("current").is_err());
    /// assert!(view.portable_method("private").is_err());
    /// let call = view.portable_method("old")?;
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("projection", json!(null)), driver.clone());
    /// assert_eq!(driver.run_until(host.root(move |ctx| call(Some(json!(7)), ctx)))?, Some(json!(7)));
    /// driver.run_until(host.close())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn project_method(
        mut self,
        alias: impl Into<String>,
        source: Rc<Self>,
        target: &str,
    ) -> Result<Self> {
        let alias = alias.into();
        let native = source.methods.get(target).cloned();
        let portable = source.json_methods.get(target).cloned();
        if native.is_none() && portable.is_none() {
            return Err(RuntimeError::new(
                crate::ErrorCode::NotInVariant,
                "method",
                "projection target does not exist",
            ));
        }
        if let Some(method) = native {
            self.methods.insert(alias.clone(), method);
        } else {
            self.methods.remove(&alias);
        }
        if let Some(method) = portable {
            self.json_methods.insert(alias, method);
        } else {
            self.json_methods.remove(&alias);
        }
        Ok(self.retain_owner(source))
    }

    /// Registers a Serde portable method on this candidate implementation.
    ///
    /// During dispatch, argument deserialization fails before calling `method`; result
    /// serialization (including non-finite-number refusal) can fail after its effects.
    /// Schema checks remain the reference
    /// dispatcher's responsibility. Registering the same name replaces this candidate's entry.
    pub fn serde_method<A: serde::de::DeserializeOwned + 'static, R: serde::Serialize + 'static>(
        self,
        name: impl Into<String>,
        method: impl Fn(A, Context) -> LocalBoxFuture<'static, Result<R>> + 'static,
    ) -> Self {
        let method = Rc::new(method);
        self.json_method(name, move |args, ctx| {
            let method = method.clone();
            async move {
                let args = args
                    .ok_or_else(|| RuntimeError::invalid("args", "method requires an argument"))?;
                let args = tembra_contracts::from_portable_value(&args)
                    .map_err(|error| RuntimeError::invalid("args", error.to_string()))?;
                let result = method(args, ctx).await?;
                tembra_contracts::to_portable_value(&result)
                    .map(Some)
                    .map_err(|error| RuntimeError::invalid("result", error.to_string()))
            }
            .boxed_local()
        })
    }

    /// Registers a portable method requiring absent arguments. Explicit null is present.
    /// Result serialization, including non-finite-number refusal, may fail after effects.
    pub fn serde_no_args<R: serde::Serialize + 'static>(
        self,
        name: impl Into<String>,
        method: impl Fn(Context) -> LocalBoxFuture<'static, Result<R>> + 'static,
    ) -> Self {
        let method = Rc::new(method);
        self.json_method(name, move |args, ctx| {
            let method = method.clone();
            async move {
                if args.is_some() {
                    return Err(RuntimeError::invalid("args", "method takes no argument"));
                }
                tembra_contracts::to_portable_value(&method(ctx).await?)
                    .map(Some)
                    .map_err(|error| RuntimeError::invalid("result", error.to_string()))
            }
            .boxed_local()
        })
    }

    /// Obtains a weak state source for an explicit compatibility view.
    ///
    /// This does not acquire a claim or validate a consumer schema. The Host validates
    /// the returned adapter view and transferred state before exposing it.
    ///
    /// # Errors
    /// Returns `not_in_variant` if this candidate has no state member with that name.
    pub fn readable(&self, name: &str) -> Result<Readable> {
        self.states.get(name).cloned().ok_or_else(|| {
            RuntimeError::new(
                crate::ErrorCode::NotInVariant,
                "state",
                "state source does not exist",
            )
        })
    }

    /// Exposes a readable as one state member of this candidate implementation.
    ///
    /// Provider schemas validate initial and later graph candidates before adoption.
    /// Forwarded external sources become local projections; they do not acquire the
    /// authority to roll back another object's writes.
    ///
    /// ```
    /// use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
    /// use futures::FutureExt;
    /// use serde_json::json;
    /// use tembra_core::*;
    /// use contracts::{Contract, Member};
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("state-example", json!(null)), driver.clone());
    /// let contract = Rc::new(Contract::local("counter", 1, json!({}), BTreeMap::from([
    ///     ("count".into(), Member::State { schema: json!({"type":"integer","minimum":0}) }),
    /// ]))?);
    /// let retained = Rc::new(RefCell::new(None::<Cell>));
    /// let saved = retained.clone();
    /// host.serve(contract.clone(), Rc::new(move |_, _, declaration| {
    ///     let cell = declaration.state("count", StateDeclaration::new(
    ///         json!({"type":"integer"}), json!(1)))?;
    ///     *saved.borrow_mut() = Some(cell.clone());
    ///     Ok(Implementation::new().state("count", cell.readable()))
    /// }), ServeOptions::default())?;
    /// let reference = driver.run_until(host.get(contract, json!({}), GetOptions::default()))?;
    /// let readable = reference.readable("count")?;
    /// driver.run_until(host.root(move |ctx| async move {
    ///     let cell = retained.borrow().as_ref().unwrap().clone();
    ///     assert!(cell.replace(&ctx, json!(-1), ReplaceOptions::default()).is_err());
    ///     assert_eq!(readable.snapshot()?.value(), &json!(1));
    ///     cell.replace(&ctx, json!(0), ReplaceOptions::default())?;
    ///     assert_eq!(readable.snapshot()?.value(), &json!(0));
    ///     Ok(())
    /// }.boxed_local()))?;
    /// driver.run_until(host.close())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn state(mut self, name: impl Into<String>, source: Readable) -> Self {
        self.states.insert(name.into(), source);
        self
    }

    pub fn opaque<T: 'static>(mut self, name: impl Into<String>, resource: Resource<T>) -> Self {
        self.opaque.insert(
            name.into(),
            Rc::new(move || resource.value().map(|value| value as Rc<dyn Any>)),
        );
        self
    }

    /// Exposes a checked native projection of an existing resource as an opaque member.
    ///
    /// Registration does not read the resource or run the mapper. Each getter first calls
    /// Resource::value, so factory, pending and closed-value refusals precede the mapper.
    /// On mapper error the getter does not close or replace the resource. Effects performed
    /// by the mapper, including interior mutation through T, are not rolled back. Returned
    /// Rc identity is preserved; this introduces no synthetic resource, raw getter or
    /// additional cleanup owner.
    ///
    /// ```
    /// use std::{cell::Cell, collections::BTreeMap, rc::Rc};
    /// use futures::FutureExt;
    /// use serde_json::json;
    /// use tembra_core::{contracts::{Contract, Member}, *};
    /// let driver = ManualDriver::new();
    /// let host = Host::new(HostOptions::new("example", json!(null)), driver.clone());
    /// let token = Rc::new(Contract::local("mapped", 1, json!({}), BTreeMap::from([
    ///     ("native".into(), Member::Opaque { id: "example.counter@1".into() }),
    /// ]))?);
    /// let refused = Rc::new(Cell::new(true));
    /// let checked = refused.clone();
    /// host.serve(token.clone(), Rc::new(move |_, _, declaration| {
    ///     let resource = declaration.resource("value", ResourceDeclaration::new(|_|
    ///         async { Ok(Rc::new(Cell::new(9_u32))) }.boxed_local()))?;
    ///     let checked = checked.clone();
    ///     Ok(Implementation::new().map_opaque("native", resource, move |value| {
    ///         if checked.get() { return Err(RuntimeError::invalid("decode", "refused")); }
    ///         Ok(value.as_ref().clone())
    ///     }))
    /// }), ServeOptions::default())?;
    /// let reference = driver.run_until(host.get(token, json!({}), GetOptions::default()))?;
    /// assert_eq!(reference.opaque::<Cell<u32>>("native").err().unwrap().reason, "decode");
    /// refused.set(false);
    /// let value = reference.opaque::<Cell<u32>>("native")?;
    /// assert_eq!(value.get(), 9); // Refusal preserved the real resource.
    /// assert!(Rc::ptr_eq(&value, &reference.opaque::<Cell<u32>>("native")?));
    /// driver.run_until(host.close())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn map_opaque<T: 'static, U: 'static>(
        mut self,
        name: impl Into<String>,
        resource: Resource<T>,
        map: impl Fn(Rc<T>) -> Result<Rc<U>> + 'static,
    ) -> Self {
        self.opaque.insert(
            name.into(),
            Rc::new(move || {
                let value = resource.value()?;
                map(value).map(|value| value as Rc<dyn Any>)
            }),
        );
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifetime {
    Referenced,
    Host,
}

#[derive(Clone)]
pub struct ServeOptions {
    pub lifetime: Lifetime,
    pub linger: std::time::Duration,
    pub accepts: BTreeMap<u64, ContractAdapter>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            lifetime: Lifetime::Referenced,
            linger: std::time::Duration::ZERO,
            accepts: BTreeMap::new(),
        }
    }
}

impl ServeOptions {
    pub fn with_adapter(mut self, version: u64, adapter: ContractAdapter) -> Self {
        self.accepts.insert(version, adapter);
        self
    }
}

pub struct Declaration {
    pub(crate) pass: Weak<FactoryScope>,
    pub(crate) host: Rc<HostInner>,
    pub(crate) object: Rc<ObjectRecord>,
}

impl Declaration {
    /// Declares a named versioned state cell owned by this object.
    ///
    /// Immediate values are readable after validation. Asynchronous initializers run
    /// after resources, declared dependencies and external hydration, in independent
    /// host-principal roots. A completed sibling is readable; a pending sibling is not.
    /// Initializers may write initialized cells outside the factory. Direct cell observers
    /// can see these writes; reference state proxies remain unhydrated until activation
    /// succeeds.
    ///
    /// Returned errors and cancellation do not release resources while another initializer
    /// is still running: activation joins all initializer futures first. Cancellation is
    /// cooperative, so a callback that never settles can delay cleanup indefinitely. Native
    /// panics propagate; this is not a panic recovery or external-effect rollback boundary.
    ///
    /// # Errors
    /// Refuses duplicate names, invalid versions/schemas and invalid immediate values.
    /// No initializer runs during this synchronous declaration pass.
    pub fn state(
        &mut self,
        name: impl Into<String>,
        declaration: StateDeclaration,
    ) -> Result<crate::Cell> {
        match self.reload_candidate()? {
            Some(candidate) => candidate.graph.declare_cell(name.into(), declaration),
            None => self.object.state.declare_cell(name.into(), declaration),
        }
    }

    /// Declares an unnamed synchronous projection of the supplied readable sources.
    ///
    /// # Errors
    /// Refuses unsupported schemas or invalid graph/source declarations. Pending sources
    /// delay initialization; a failed internal projection refuses its complete candidate.
    pub fn derive(
        &mut self,
        sources: Vec<Readable>,
        schema: Value,
        compute: impl Fn(&[Snapshot]) -> Result<Value> + 'static,
    ) -> Result<Readable> {
        match self.reload_candidate()? {
            Some(candidate) => candidate.graph.derive(sources, schema, Rc::new(compute)),
            None => self.object.state.derive(sources, schema, Rc::new(compute)),
        }
    }

    /// Declares an object-owned reference, deduplicated by object address.
    ///
    /// # Errors
    /// Refuses incompatible bindings, two consumer tokens at one address and dependency
    /// cycles. Readiness is deferred; this callback must not invoke the reference.
    pub fn get(
        &mut self,
        contract: Rc<Contract>,
        input: Value,
        options: crate::GetOptions,
    ) -> Result<Reference> {
        match self.reload_candidate()? {
            Some(candidate) => {
                self.host
                    .declare_reload_get(&self.object, &candidate, contract, input, options)
            }
            None => self
                .host
                .declare_get(&self.object, contract, input, options),
        }
    }

    /// Adds one native resource to this object's activation candidate.
    ///
    /// # Errors
    /// Refuses invalid versions, empty/duplicate names and dependencies not declared by this
    /// object (resource dependencies must appear earlier). Refusal leaves the resource list
    /// unchanged and never invokes its opener.
    pub fn resource<T: 'static>(
        &mut self,
        name: impl Into<String>,
        declaration: ResourceDeclaration<T>,
    ) -> Result<Resource<T>> {
        let name = name.into();
        if let Some(candidate) = self.reload_candidate()? {
            for dependency in &declaration.deps {
                if let crate::ResourceDependency::Reference(reference) = dependency
                    && !candidate
                        .dependencies
                        .borrow()
                        .values()
                        .any(|own| own.same_claim(reference))
                {
                    return Err(RuntimeError::invalid(
                        "resource_dependency",
                        "reference dependency must belong to this candidate",
                    ));
                }
            }
            return candidate.resources.declare(name, declaration);
        }
        if declaration.version == 0 || declaration.version > 9_007_199_254_740_991 {
            return Err(RuntimeError::invalid(
                "resource_version",
                "resource version must be a positive safe integer",
            ));
        }
        if name.is_empty()
            || self
                .object
                .resources
                .borrow()
                .iter()
                .any(|resource| resource.name == name)
        {
            return Err(RuntimeError::invalid(
                "duplicate",
                "resource names must be nonempty and unique",
            ));
        }
        for dependency in &declaration.deps {
            match dependency {
                crate::ResourceDependency::Resource(resource) => {
                    if !self
                        .object
                        .resources
                        .borrow()
                        .iter()
                        .any(|own| Rc::ptr_eq(own, &resource.0))
                    {
                        return Err(RuntimeError::invalid(
                            "resource_dependency",
                            "resource dependency must be declared earlier by this object",
                        ));
                    }
                }
                crate::ResourceDependency::Reference(reference) => {
                    if !self
                        .object
                        .dependencies
                        .borrow()
                        .values()
                        .any(|own| own.same_claim(reference))
                    {
                        return Err(RuntimeError::invalid(
                            "resource_dependency",
                            "reference dependency must belong to this object",
                        ));
                    }
                }
            }
        }
        let resource = ResourceRecord::declare(name, declaration, Rc::downgrade(&self.host));
        self.object
            .resources
            .borrow_mut()
            .push(resource.record.clone());
        Ok(resource)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Activating,
    Live,
    Disposing,
    Disposed,
    Failed,
}

pub(crate) struct ObjectRecord {
    pub id: u64,
    pub(crate) service_ordinal: u64,
    pub(crate) activation_service: RefCell<Option<Rc<crate::host::Service>>>,
    pub(crate) installed: RefCell<BTreeMap<u64, Rc<crate::host::reload::InstalledVersion>>>,
    pub(crate) current_epoch: Cell<u64>,
    pub(crate) min_code_version: Cell<u64>,
    pub(crate) gate: RefCell<Option<std::collections::HashSet<u64>>>,
    pub(crate) activities: Rc<crate::activity::ActivityDeclarations>,

    pub(crate) startup: Option<Rc<crate::host::facets::HostActivation>>,
    pub(crate) registrations: RefCell<Vec<crate::registry::RegistrationDeclaration>>,
    pub key: crate::host::Address,
    pub input: Value,
    pub principal: Value,
    pub contract: Rc<Contract>,
    // Service callbacks belong to installed versions, not terminal reference records.
    pub lifetime: Lifetime,
    pub linger: std::time::Duration,
    pub factory: RefCell<Option<Factory>>,
    pub state: Rc<StateGraph>,
    pub state_exposures: RefCell<Vec<Exposure>>,
    pub phase: Cell<Phase>,
    pub error: RefCell<Option<RuntimeError>>,
    pub implementation: RefCell<Option<Rc<Implementation>>>,
    pub adapter_views: RefCell<HashMap<String, Rc<Implementation>>>,
    pub dependencies: RefCell<HashMap<crate::host::Address, Reference>>,
    pub resources: RefCell<Vec<Rc<ResourceRecord>>>,
    pub opened: RefCell<Vec<Rc<ResourceRecord>>>,
    pub claims: RefCell<Vec<Weak<Claim>>>,
    pub references: Cell<usize>,
    pub invocations: Cell<usize>,
    pub next_invocation: Cell<u64>,
    pub active_contexts: RefCell<HashMap<u64, crate::invocation::ActiveInvocation>>,
    pub ready_waiters: RefCell<HashMap<u64, futures::channel::oneshot::Sender<Result<()>>>>,
    pub next_waiter: Cell<u64>,
    pub dispose_waiters: RefCell<Vec<futures::channel::oneshot::Sender<()>>>,
    pub idle_waiters: RefCell<Vec<futures::channel::oneshot::Sender<()>>>,
    pub cancellation: crate::CancellationToken,
    pub linger_generation: Cell<u64>,
    pub host_pin: Cell<bool>,
}

impl ObjectRecord {
    pub fn finish_ready(&self, result: Result<()>) {
        let waiters = std::mem::take(&mut *self.ready_waiters.borrow_mut());
        for waiter in waiters.into_values() {
            let _ = waiter.send(result.clone());
        }
        self.notify_claims();
    }

    pub fn notify_claims(&self) {
        let claims: Vec<_> = self
            .claims
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for claim in claims {
            claim.notify();
        }
    }

    pub async fn ready(self: &Rc<Self>) -> Result<()> {
        match self.phase.get() {
            Phase::Live => return Ok(()),
            Phase::Activating => {}
            _ => {
                return Err(self.error.borrow().clone().unwrap_or_else(|| {
                    RuntimeError::new(
                        crate::ErrorCode::Gone,
                        "disposed",
                        "object is no longer live",
                    )
                }));
            }
        }
        let (send, receive) = futures::channel::oneshot::channel();
        let id = self.next_waiter.get() + 1;
        self.next_waiter.set(id);
        self.ready_waiters.borrow_mut().insert(id, send);
        let _waiter = ReadyWaiter {
            object: Rc::downgrade(self),
            id,
        };
        receive.await.unwrap_or_else(|_| {
            Err(RuntimeError::unavailable(
                "activation",
                "activation ended without readiness",
            ))
        })
    }

    pub async fn disposed(&self) {
        if self.phase.get() == Phase::Disposed {
            return;
        }
        let (send, receive) = futures::channel::oneshot::channel();
        self.dispose_waiters.borrow_mut().push(send);
        let _ = receive.await;
    }
}

struct ReadyWaiter {
    object: Weak<ObjectRecord>,
    id: u64,
}
impl Drop for ReadyWaiter {
    fn drop(&mut self) {
        if let Some(object) = self.object.upgrade() {
            object.ready_waiters.borrow_mut().remove(&self.id);
        }
    }
}
