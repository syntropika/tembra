//! Validate executable declarations before any adapter publishes kernel metadata.
use crate::{Binding, Contract, Error, javascript_name, service_name};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use wit_parser::WorldItem;

/// A guest-declared executable bundle before it is matched against the selected world.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Implementation {
    pub name: Option<String>,
    pub api_version: Option<u32>,
    pub services: BTreeMap<String, ServiceImplementation>,
}

/// One guest service keyed by its JavaScript alias.
#[derive(Deserialize, Serialize)]
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

impl Contract {
    /// Require the exact identity, services and callbacks declared by the selected world.
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
        let mut services = crate::lower::services(&self.resolve, self.world, name)?
            .into_iter()
            .map(|service| (service.name.clone(), service))
            .collect::<BTreeMap<_, _>>();
        let mut provided = implementation.services;
        let mut plan = Vec::new();
        for item in self.resolve.worlds[self.world].exports.values() {
            let WorldItem::Interface { id, .. } = item else {
                continue;
            };
            // Lowering already rejected anonymous exports and skipped callback-free interfaces.
            let Some(interface) = self.resolve.id_of(*id) else {
                continue;
            };
            let Some(service) = services.remove(&service_name(name, &interface)) else {
                continue;
            };
            let alias = javascript_name(
                self.resolve.interfaces[*id]
                    .name
                    .as_deref()
                    .unwrap_or_default(),
            );
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
    pub fn import_services(
        &self,
        bindings: &BTreeMap<String, Binding>,
    ) -> Result<BTreeMap<String, String>, Error> {
        let mut result = BTreeMap::new();
        for (key, item) in &self.resolve.worlds[self.world].imports {
            let WorldItem::Interface { id, .. } = item else {
                continue;
            };
            let binding = self.resolve.name_world_key(key);
            let Some(Binding::Orb { name, .. }) = bindings.get(&binding) else {
                continue;
            };
            let contract = self.resolve.id_of(*id).ok_or_else(|| {
                Error("anonymous import needs an explicit provider mapping".into())
            })?;
            result.insert(binding, service_name(name, &contract));
        }
        Ok(result)
    }

    /// Resolve a JavaScript import method through the original WIT names, never by guessing.
    pub fn import_method(&self, binding: &str, method: &str) -> Result<String, Error> {
        for (key, item) in &self.resolve.worlds[self.world].imports {
            if self.resolve.name_world_key(key) != binding {
                continue;
            }
            if let WorldItem::Interface { id, .. } = item {
                let matching = self.resolve.interfaces[*id]
                    .functions
                    .keys()
                    .filter(|name| javascript_name(name) == method)
                    .collect::<Vec<_>>();
                if let [name] = matching.as_slice() {
                    return Ok((*name).clone());
                }
            }
        }
        Err(Error(format!(
            "unknown or ambiguous WIT import method {binding}#{method}"
        )))
    }
}
