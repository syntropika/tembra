//! Checked keyed-reference roles: typed, host-local views of an Orb provider's service.
//!
//! A compiled Orb may import a *reference view* instead of a provider service. The view is an
//! ordinary WIT interface with one locally defined resource `R`, whose methods mirror the
//! provider service's functions, and one asynchronous acquisition function
//! `func(key: list<string>) -> result<own<R>, string>`. An artifact role maps the view binding
//! to the provider's interface identity; only that explicit, checked role distinguishes a view
//! from an identically shaped provider-owned WIT resource interface.
//!
//! A view handle names a kernel object `(service, key, root principal)` held by the host. It is a
//! local host reference capability, never a provider-owned inter-Orb resource, so its methods
//! carry data only and use the ordinary value profile. The original WIT stays intact for
//! component verification, bind slots and native receiver typing; requirements and digests use
//! the derived provider service instead.
use crate::{
    Error, Field, Function, FunctionKind, Interface, Item, Kind, PreparedContract,
    ResourceIdentity, Ty,
};
use serde::Serialize;
use std::collections::BTreeMap;

/// One checked reference view, derived from the importer's WIT and its artifact role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceBinding {
    /// Versioned identity of the provider interface the view addresses.
    pub interface: String,
    /// Nominal identity of the view resource declared by the importer.
    pub resource: ResourceIdentity,
    /// Original WIT name of the freestanding acquisition function.
    pub acquire: String,
    /// Original WIT method names (`[method]R.name`) mapped to provider function names.
    pub methods: BTreeMap<String, String>,
    /// Interface digest of the derived provider service, used as the requirement signature.
    pub signature: String,
}

impl PreparedContract {
    /// Check reference roles against this contract and attach the derived bindings.
    pub(crate) fn with_references(
        mut self,
        roles: &BTreeMap<String, String>,
    ) -> Result<Self, Error> {
        let references = self.reference_bindings(roles)?;
        self.set_references(references);
        Ok(self)
    }

    /// Derive and check every role; a malformed role refuses the whole artifact.
    pub(crate) fn reference_bindings(
        &self,
        roles: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, ReferenceBinding>, Error> {
        roles
            .iter()
            .map(|(binding, target)| {
                self.reference_binding(binding, target)
                    .map(|reference| (binding.clone(), reference))
                    .map_err(|error| Error(format!("reference view {binding}: {error}")))
            })
            .collect()
    }

    fn reference_binding(&self, binding: &str, target: &str) -> Result<ReferenceBinding, Error> {
        crate::model::identity(target)?;
        let interface = self
            .model()
            .imports
            .iter()
            .find_map(|entry| match &entry.item {
                Item::Interface(interface) if entry.key == binding => Some(interface),
                _ => None,
            })
            .ok_or_else(|| Error("the role names no imported interface".into()))?;
        let contract = interface
            .contract
            .as_deref()
            .ok_or_else(|| Error("an anonymous interface cannot be a reference view".into()))?;
        if contract == target {
            return Err(Error("a reference view cannot address itself".into()));
        }
        let mut resources = interface.resources.iter();
        let (Some((local, &resource)), None) = (resources.next(), resources.next()) else {
            return Err(Error(
                "a reference view declares exactly one resource".into(),
            ));
        };
        let identity = match self.kind(resource) {
            Kind::Resource(
                identity @ ResourceIdentity {
                    interface: Some(owner),
                    name: Some(name),
                },
            ) if owner == contract && name == local => identity,
            _ => {
                return Err(Error(
                    "the view resource must be defined by the view interface".into(),
                ));
            }
        };
        let prefix = format!("[method]{local}.");
        let mut acquire = None;
        let mut methods = BTreeMap::new();
        let mut derived = Vec::new();
        for function in &interface.functions {
            let kind = match function.kind {
                FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => {
                    if acquire.replace(function).is_some() {
                        return Err(Error(
                            "a reference view declares exactly one acquisition function".into(),
                        ));
                    }
                    continue;
                }
                // The derived kind keeps the declared one: the digest distinguishes them, so a
                // view method is `async` exactly when the provider function is.
                FunctionKind::Method(owner) => (owner, FunctionKind::Freestanding),
                FunctionKind::AsyncMethod(owner) => (owner, FunctionKind::AsyncFreestanding),
                other => {
                    return Err(Error(format!(
                        "{}: a reference view admits only methods and one acquisition \
                         function; found {}",
                        function.name,
                        other.as_str()
                    )));
                }
            };
            let (owner, derived_kind) = kind;
            let name = function
                .name
                .strip_prefix(&prefix)
                .filter(|_| self.resolved(Ty::Ref(owner)) == Ty::Ref(resource))
                .ok_or_else(|| Error(format!("{}: method of another resource", function.name)))?;
            let params = self.view_receiver(function, resource)?;
            for param in params {
                self.supported(param.ty, 0)
                    .map_err(|error| Error(format!("{}: {error}", function.name)))?;
            }
            if let Some(result) = function.result {
                self.supported(result, 0)
                    .map_err(|error| Error(format!("{}: {error}", function.name)))?;
            }
            methods.insert(function.name.clone(), name.to_owned());
            derived.push(Function {
                name: name.to_owned(),
                kind: derived_kind,
                params: params.to_vec(),
                result: function.result,
            });
        }
        let acquire = acquire
            .ok_or_else(|| Error("a reference view requires one acquisition function".into()))?;
        self.check_acquire(acquire, resource, local)?;
        if methods.is_empty() {
            return Err(Error(
                "a reference view requires at least one method".into(),
            ));
        }
        let signature = self.digest(&Interface {
            contract: Some(target.to_owned()),
            name: None,
            resources: BTreeMap::new(),
            explicit: true,
            functions: derived,
        })?;
        Ok(ReferenceBinding {
            interface: target.to_owned(),
            resource: identity.clone(),
            acquire: acquire.name.clone(),
            methods,
            signature,
        })
    }

    /// Require the implicit `self: borrow<R>` and return the data parameters after it.
    fn view_receiver<'a>(
        &self,
        function: &'a Function,
        resource: u32,
    ) -> Result<&'a [Field], Error> {
        let receiver = function.params.first().map(|param| self.resolved(param.ty));
        match receiver {
            Some(Ty::Ref(id))
                if matches!(self.kind(id), Kind::Borrow(borrowed)
                    if self.resolved(Ty::Ref(*borrowed)) == Ty::Ref(resource)) =>
            {
                Ok(&function.params[1..])
            }
            _ => Err(Error(format!(
                "{}: a view method requires its implicit self borrow",
                function.name
            ))),
        }
    }

    /// Require exactly `async func(<key>: list<string>) -> result<own<R>, string>`.
    fn check_acquire(&self, function: &Function, resource: u32, local: &str) -> Result<(), Error> {
        let reference = |ty: Ty| match self.resolved(ty) {
            Ty::Ref(id) => Some(self.kind(id)),
            _ => None,
        };
        let key = matches!(
            function.params.as_slice(),
            [param] if matches!(reference(param.ty), Some(Kind::List(element))
                if self.resolved(*element) == Ty::String)
        );
        let result = function.result.and_then(reference).is_some_and(|kind| {
            matches!(kind, Kind::Result { ok: Some(ok), err: Some(err) }
                if self.resolved(*err) == Ty::String
                    && matches!(reference(*ok), Some(Kind::Own(owned))
                        if self.resolved(Ty::Ref(*owned)) == Ty::Ref(resource)))
        });
        if function.kind != FunctionKind::AsyncFreestanding || !key || !result {
            return Err(Error(format!(
                "{}: the acquisition function must be \
                 `async func(key: list<string>) -> result<{local}, string>`",
                function.name
            )));
        }
        Ok(())
    }
}
