//! Explicit provider bindings and lowering into the existing kernel metadata model.
use crate::{Error, describe::has_resource, service_name};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tembra_core::{Orb, Requirement, Service};
use wit_parser::{FunctionKind, Resolve, WorldId, WorldItem};

/// An installation chooses a provider; a WIT import alone never grants authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Binding {
    Orb {
        name: String,
        api_version: u32,
        world: String,
    },
    Host {
        provider: String,
    },
}

/// A host capability to resolve against separately configured providers and grants.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostImport {
    pub binding: String,
    pub contract: String,
    pub provider: String,
}

/// Prepared metadata retains host imports separately from executable Orb requirements.
#[derive(Debug)]
pub struct Plan {
    pub orb: Orb,
    pub host_imports: Vec<HostImport>,
}

impl Plan {
    /// Encode prepared metadata for platform facades without serializing executable callbacks.
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({"orb":{
            "name":self.orb.name,"apiVersion":self.orb.api_version,
            "services":self.orb.services.iter().map(|service| serde_json::json!({"name":service.name,"methods":service.methods,"lifetime":service.lifetime.as_str(),"linger":service.linger_ms})).collect::<Vec<_>>(),
            "requires":self.orb.requires.iter().map(|requirement| serde_json::json!({"orb":requirement.orb,"apiVersion":requirement.api_version,"services":requirement.methods,"hooks":requirement.hooks})).collect::<Vec<_>>(),"hooks":self.orb.hooks},"hostImports":self.host_imports})
    }
}

/// Lower supported exported interfaces without inventing resource lifecycle semantics.
fn services(resolve: &Resolve, world: WorldId, provider: &str) -> Result<Vec<Service>, Error> {
    let mut services = Vec::new();
    for (key, item) in &resolve.worlds[world].exports {
        match item {
            WorldItem::Interface { id, .. } => {
                let contract = resolve.id_of(*id).ok_or_else(|| {
                    Error("anonymous export needs an explicit service mapping".into())
                })?;
                let interface = &resolve.interfaces[*id];
                if resolve.name_world_key(key) != contract {
                    return Err(Error(
                        "export aliases require an explicit service mapping".into(),
                    ));
                }
                if has_resource(resolve, *id)
                    || !interface.functions.values().all(|function| {
                        matches!(
                            function.kind,
                            FunctionKind::Freestanding | FunctionKind::AsyncFreestanding
                        )
                    })
                {
                    return Err(Error(format!(
                        "{contract}: resource exports require a lifecycle adapter"
                    )));
                }
                if !interface.functions.is_empty() {
                    services.push(Service::new(
                        service_name(provider, &contract),
                        interface.functions.keys().cloned(),
                    ));
                }
            }
            WorldItem::Type { .. } => {}
            WorldItem::Function(_) => {
                return Err(Error(
                    "world function exports need an explicit service mapping".into(),
                ));
            }
        }
    }
    Ok(services)
}

/// Build requirements from the resolved graph, checking each selected provider world.
pub(crate) fn plan(
    resolve: &Resolve,
    world: WorldId,
    name: &str,
    api_version: u32,
    bindings: &BTreeMap<String, Binding>,
) -> Result<Plan, Error> {
    if name.trim().is_empty() || api_version == 0 {
        return Err(Error(
            "registration must supply an Orb identity and positive API version".into(),
        ));
    }
    let mut orb = Orb::new(name, api_version);
    orb.services = services(resolve, world, name)?;
    let mut requirements = BTreeMap::<String, Requirement>::new();
    let mut host_imports = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    for (key, item) in &resolve.worlds[world].imports {
        let WorldItem::Interface { id, .. } = item else {
            if matches!(item, WorldItem::Type { .. }) {
                continue;
            }
            return Err(Error(
                "world function imports need an explicit provider mapping".into(),
            ));
        };
        let interface = &resolve.interfaces[*id];
        if interface.functions.is_empty() && !has_resource(resolve, *id) {
            continue;
        }
        let contract = resolve
            .id_of(*id)
            .ok_or_else(|| Error("anonymous import needs an explicit provider mapping".into()))?;
        let binding = resolve.name_world_key(key);
        let selected = bindings
            .get(&binding)
            .ok_or_else(|| Error(format!("missing provider for {binding}")))?;
        used.insert(binding.clone());
        match selected {
            Binding::Host { provider } => {
                if provider.trim().is_empty() {
                    return Err(Error(format!("empty host provider for {binding}")));
                }
                host_imports.push(HostImport {
                    binding,
                    contract,
                    provider: provider.clone(),
                });
            }
            Binding::Orb {
                name,
                api_version,
                world: provider_world,
            } => {
                if name.trim().is_empty() || *api_version == 0 {
                    return Err(Error(format!("invalid Orb provider for {binding}")));
                }
                let packages = resolve.worlds[world]
                    .package
                    .into_iter()
                    .collect::<Vec<_>>();
                let provider_world = resolve
                    .select_world(&packages, Some(provider_world))
                    .map_err(|error| Error(format!("{error:#}")))?;
                let available = services(resolve, provider_world, name)?;
                let service = service_name(name, &contract);
                let offered = available
                    .iter()
                    .find(|item| item.name == service)
                    .ok_or_else(|| {
                        Error(format!(
                            "selected provider {name} does not export {contract}"
                        ))
                    })?;
                let requirement = requirements
                    .entry(name.clone())
                    .or_insert_with(|| Requirement {
                        orb: name.clone(),
                        api_version: *api_version,
                        methods: BTreeMap::new(),
                        hooks: Default::default(),
                    });
                if requirement.api_version != *api_version {
                    return Err(Error(format!(
                        "inconsistent API version for provider {name}"
                    )));
                }
                requirement.methods.insert(service, offered.methods.clone());
            }
        }
    }
    if let Some(unused) = bindings.keys().find(|name| !used.contains(*name)) {
        return Err(Error(format!("unused provider binding {unused}")));
    }
    orb.requires = requirements.into_values().collect();
    Ok(Plan { orb, host_imports })
}
