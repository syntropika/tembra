//! Validate executable declarations before any adapter publishes kernel metadata.
use crate::{Binding, Error, PreparedContract, javascript_name, model::Item, service_name};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A guest-declared executable bundle before it is matched against the selected world.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Implementation {
    pub name: Option<String>,
    pub api_version: Option<u32>,
    pub services: BTreeMap<String, ServiceImplementation>,
    #[serde(default)]
    pub hooks: BTreeMap<String, crate::HookComposition>,
    #[serde(default)]
    pub contributes: BTreeMap<String, i32>,
}

/// One guest service keyed by its JavaScript alias.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceImplementation {
    pub methods: BTreeSet<String>,
    pub lifetime: Option<Lifetime>,
    pub linger: Option<u32>,
    #[serde(default)]
    pub access: bool,
}

/// Serde mirror of the kernel ownership policy, keeping the lifecycle kernel serde-free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifetime {
    #[default]
    Referenced,
    Host,
}

impl From<Lifetime> for tembra_core::Lifetime {
    /// Map the wire policy onto the kernel variant without a string round trip.
    fn from(lifetime: Lifetime) -> Self {
        match lifetime {
            Lifetime::Referenced => Self::Referenced,
            Lifetime::Host => Self::Host,
        }
    }
}

/// A checked guest service ready to be installed by a native or SDK adapter.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceBinding {
    /// Provider-qualified kernel service name.
    pub name: String,
    /// Versioned WIT interface identity used for boundary validation.
    pub interface: String,
    /// Guest-facing JavaScript service alias.
    pub alias: String,
    /// Original WIT method names mapped to their guest-facing JavaScript names.
    pub methods: BTreeMap<String, String>,
    pub lifetime: Lifetime,
    pub linger: u32,
    /// Canonical interface digest propagated to kernel ABI matching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// The checked host-local incoming HTTP handler of an implementation.
///
/// It is never a [`ServiceBinding`]: it has no portable slot and no signature, and adapters
/// register it only as a host-local service that the host's ingress entry point calls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressBinding {
    /// Host-local kernel service name, `<orb>::wasi:http/handler@0.3.0`.
    pub name: String,
    /// The exact handler identity, [`crate::HTTP_HANDLER`].
    pub interface: String,
    /// Guest-facing JavaScript service alias of the export.
    pub alias: String,
    /// Guest-facing JavaScript spelling of the WIT `handle` function.
    pub method: String,
    pub lifetime: Lifetime,
    pub linger: u32,
}

impl PreparedContract {
    /// Check the implementation of the incoming HTTP handler, if this world exports one.
    ///
    /// The implementation must declare exactly one service under the export's alias with exactly
    /// the `handle` method, and no access policy (the handler is called only by the host). Its
    /// alias may not collide with another export's. Worlds without the handler return `None`;
    /// an implementation that still declares such a service is refused by
    /// [`Self::check_implementation`] as a service absent from its world.
    pub fn check_ingress(
        &self,
        name: &str,
        implementation: &Implementation,
    ) -> Result<Option<IngressBinding>, Error> {
        let Some(entry) = self
            .model()
            .exports
            .iter()
            .find(|entry| crate::lower::is_ingress(entry))
        else {
            return Ok(None);
        };
        let Item::Interface(interface) = &entry.item else {
            unreachable!("recognized as an interface")
        };
        let alias = javascript_name(interface.name.as_deref().unwrap_or_default());
        let collision = self.model().exports.iter().any(|other| {
            !std::ptr::eq(other, entry)
                && matches!(&other.item, Item::Interface(other)
                    if javascript_name(other.name.as_deref().unwrap_or_default()) == alias)
        });
        if collision {
            return Err(Error(format!(
                "{}: ambiguous JavaScript service {alias}",
                crate::HTTP_HANDLER
            )));
        }
        let method = javascript_name(&interface.functions[0].name);
        let Some(service) = implementation.services.get(&alias) else {
            return Err(Error(format!(
                "{}: missing service implementation {alias}",
                crate::HTTP_HANDLER
            )));
        };
        if service.methods != BTreeSet::from([method.clone()]) {
            return Err(Error(format!(
                "{}: the handler implements exactly {method:?}, not {:?}",
                crate::HTTP_HANDLER,
                service.methods
            )));
        }
        if service.access {
            return Err(Error(format!(
                "{}: the incoming HTTP handler has no access policy; only the host calls it",
                crate::HTTP_HANDLER
            )));
        }
        if implementation.hooks.contains_key(&alias)
            || implementation.contributes.contains_key(crate::HTTP_HANDLER)
        {
            return Err(Error(format!(
                "{}: the incoming HTTP handler cannot be a hook",
                crate::HTTP_HANDLER
            )));
        }
        Ok(Some(IngressBinding {
            name: service_name(name, crate::HTTP_HANDLER),
            interface: crate::HTTP_HANDLER.into(),
            alias,
            method,
            lifetime: service.lifetime.unwrap_or_default(),
            linger: service.linger.unwrap_or(0),
        }))
    }

    /// Require the exact identity, services and callbacks declared by the selected world.
    ///
    /// The incoming HTTP handler is checked by [`Self::check_ingress`] and is not returned as an
    /// ordinary service.
    pub fn check_implementation(
        &self,
        name: &str,
        api_version: u32,
        implementation: Implementation,
    ) -> Result<Vec<ServiceBinding>, Error> {
        if implementation
            .name
            .as_deref()
            .is_some_and(|value| value != name)
            || implementation
                .api_version
                .is_some_and(|value| value != api_version)
        {
            return Err(Error(
                "bundle identity differs from its registration".into(),
            ));
        }
        let mut services = self
            .services(name)?
            .into_iter()
            .map(|service| (service.name.clone(), service))
            .collect::<BTreeMap<_, _>>();
        let ingress = self.check_ingress(name, &implementation)?;
        let mut provided = implementation.services;
        if let Some(ingress) = ingress {
            provided.remove(&ingress.alias);
        }
        let hook_exports = self.hook_exports(&implementation.hooks, &implementation.contributes)?;
        let mut plan = Vec::new();
        for entry in &self.model().exports {
            let Item::Interface(declared) = &entry.item else {
                continue;
            };
            // Lowering already rejected anonymous exports and skipped callback-free interfaces.
            let Some(interface) = declared.contract.clone() else {
                continue;
            };
            let Some(service) = services.remove(&service_name(name, &interface)) else {
                continue;
            };
            let alias = javascript_name(declared.name.as_deref().unwrap_or_default());
            if hook_exports.contains(&interface) {
                if provided.contains_key(&alias) {
                    return Err(Error(format!(
                        "{interface}: hook callbacks cannot also declare a service"
                    )));
                }
                continue;
            }
            let Some(implementation) = provided.remove(&alias) else {
                return Err(Error(format!(
                    "{interface}: missing service implementation {alias}"
                )));
            };
            let methods = service
                .methods
                .into_iter()
                .map(|method| {
                    let spelling = javascript_name(&method);
                    (method, spelling)
                })
                .collect::<BTreeMap<_, _>>();
            let expected = methods.values().cloned().collect::<BTreeSet<_>>();
            if implementation.methods != expected {
                return Err(Error(format!(
                    "{interface}: implementation methods differ from WIT; missing {:?}; extra {:?}",
                    expected
                        .difference(&implementation.methods)
                        .collect::<Vec<_>>(),
                    implementation
                        .methods
                        .difference(&expected)
                        .collect::<Vec<_>>()
                )));
            }
            if implementation.access {
                return Err(Error(format!(
                    "{interface}: service access policies require a prepared view adapter"
                )));
            }
            plan.push(ServiceBinding {
                name: service.name,
                interface,
                alias,
                methods,
                lifetime: implementation.lifetime.unwrap_or_default(),
                linger: implementation.linger.unwrap_or(0),
                signature: service.signature,
            });
        }
        if !provided.is_empty() {
            return Err(Error(format!(
                "bundle exports services absent from its WIT world: {:?}",
                provided.keys().collect::<Vec<_>>()
            )));
        }
        Ok(plan)
    }

    /// Map each Orb-provided import binding to the qualified service its proxies call.
    ///
    /// A checked reference view maps to the provider interface its role addresses, not to the
    /// view interface itself.
    pub fn import_services(
        &self,
        bindings: &BTreeMap<String, Binding>,
    ) -> Result<BTreeMap<String, String>, Error> {
        let mut result = BTreeMap::new();
        for entry in &self.model().imports {
            let Item::Interface(interface) = &entry.item else {
                continue;
            };
            let Some(Binding::Orb { name, .. }) = bindings.get(&entry.key) else {
                continue;
            };
            let contract = match self.references().get(&entry.key) {
                Some(reference) => reference.interface.as_str(),
                None => interface.contract.as_deref().ok_or_else(|| {
                    Error("anonymous import needs an explicit provider mapping".into())
                })?,
            };
            result.insert(entry.key.clone(), service_name(name, contract));
        }
        Ok(result)
    }
}
