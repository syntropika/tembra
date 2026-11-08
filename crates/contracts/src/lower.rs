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
    Orb { name: String, api_version: u32 },
    Host { provider: String },
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanDescription<'a> {
    orb: OrbDescription<'a>,
    host_imports: &'a [HostImport],
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OrbDescription<'a> {
    name: &'a str,
    api_version: u32,
    services: Vec<ServiceDescription<'a>>,
    requires: Vec<RequirementDescription<'a>>,
    hooks: &'a std::collections::BTreeSet<String>,
}
#[derive(Serialize)]
struct ServiceDescription<'a> {
    name: &'a str,
    methods: &'a std::collections::BTreeSet<String>,
    lifetime: &'static str,
    linger: u32,
    signature: &'a Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RequirementDescription<'a> {
    orb: &'a str,
    api_version: u32,
    services: &'a BTreeMap<String, std::collections::BTreeSet<String>>,
    signatures: &'a BTreeMap<String, String>,
    hooks: &'a std::collections::BTreeSet<String>,
}

impl Plan {
    /// Serialize typed metadata without retaining executable callback values.
    pub fn describe(&self) -> serde_json::Value {
        serde_json::to_value(PlanDescription {
            host_imports: &self.host_imports,
            orb: OrbDescription {
                name: &self.orb.name,
                api_version: self.orb.api_version,
                hooks: &self.orb.hooks,
                services: self
                    .orb
                    .services
                    .iter()
                    .map(|service| ServiceDescription {
                        name: &service.name,
                        methods: &service.methods,
                        lifetime: service.lifetime.as_str(),
                        linger: service.linger_ms,
                        signature: &service.signature,
                    })
                    .collect(),
                requires: self
                    .orb
                    .requires
                    .iter()
                    .map(|requirement| RequirementDescription {
                        orb: &requirement.orb,
                        api_version: requirement.api_version,
                        services: &requirement.methods,
                        signatures: &requirement.signatures,
                        hooks: &requirement.hooks,
                    })
                    .collect(),
            },
        })
        .expect("serializable Orb plan")
    }
}

/// Lower supported exported interfaces without inventing resource lifecycle semantics.
pub(crate) fn services(
    resolve: &Resolve,
    world: WorldId,
    provider: &str,
) -> Result<Vec<Service>, Error> {
    let mut services = Vec::new();
    let mut aliases = BTreeMap::new();
    for (key, item) in &resolve.worlds[world].exports {
        match item {
            WorldItem::Interface { id, .. } => {
                let contract = resolve.id_of(*id).ok_or_else(|| {
                    Error("anonymous export needs an explicit service mapping".into())
                })?;
                let interface = &resolve.interfaces[*id];
                let alias = crate::javascript_name(interface.name.as_deref().unwrap_or_default());
                if let Some(previous) = aliases.insert(alias.clone(), contract.clone()) {
                    return Err(Error(format!(
                        "ambiguous JavaScript service {alias}: {previous} and {contract}"
                    )));
                }
                let mut methods = BTreeMap::new();
                for method in interface.functions.keys() {
                    let spelling = crate::javascript_name(method);
                    if let Some(previous) = methods.insert(spelling.clone(), method) {
                        return Err(Error(format!(
                            "{contract}: ambiguous JavaScript method {spelling}: {previous} and {method}"
                        )));
                    }
                }
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
                    let mut service = Service::new(
                        service_name(provider, &contract),
                        interface.functions.keys().cloned(),
                    );
                    service.signature = Some(crate::signatures::digest(resolve, *id)?);
                    services.push(service);
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
            Binding::Orb { name, api_version } => {
                if name.trim().is_empty() || *api_version == 0 {
                    return Err(Error(format!("invalid Orb provider for {binding}")));
                }
                let service = service_name(name, &contract);
                let requirement = requirements
                    .entry(name.clone())
                    .or_insert_with(|| Requirement {
                        signatures: Default::default(),
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
                requirement
                    .signatures
                    .insert(service.clone(), crate::signatures::digest(resolve, *id)?);
                requirement
                    .methods
                    .insert(service, interface.functions.keys().cloned().collect());
            }
        }
    }
    if let Some(unused) = bindings.keys().find(|name| !used.contains(*name)) {
        return Err(Error(format!("unused provider binding {unused}")));
    }
    orb.requires = requirements.into_values().collect();
    Ok(Plan { orb, host_imports })
}
