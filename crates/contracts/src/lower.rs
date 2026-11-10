//! Explicit provider bindings and lowering into the existing kernel metadata model.
use crate::{
    Error,
    model::{Entry, Interface, Item, PreparedContract},
    service_name,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tembra_core::{Orb, Requirement, Service};

/// An installation chooses a provider; a WIT import alone never grants authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    hook_signatures: &'a BTreeMap<String, String>,
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
    hook_signatures: &'a BTreeMap<String, String>,
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
                hook_signatures: &self.orb.hook_signatures,
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
                        hook_signatures: &requirement.hook_signatures,
                    })
                    .collect(),
            },
        })
        .expect("serializable Orb plan")
    }
}

impl PreparedContract {
    /// Lower supported exported interfaces without inventing resource lifecycle semantics.
    pub(crate) fn services(&self, provider: &str) -> Result<Vec<Service>, Error> {
        let mut services = Vec::new();
        let mut aliases = BTreeMap::new();
        for Entry { key, item } in &self.model().exports {
            match item {
                Item::Interface(interface) => {
                    let contract = interface.contract.clone().ok_or_else(|| {
                        Error("anonymous export needs an explicit service mapping".into())
                    })?;
                    let alias =
                        crate::javascript_name(interface.name.as_deref().unwrap_or_default());
                    if let Some(previous) = aliases.insert(alias.clone(), contract.clone()) {
                        return Err(Error(format!(
                            "ambiguous JavaScript service {alias}: {previous} and {contract}"
                        )));
                    }
                    let mut methods = BTreeMap::new();
                    for function in &interface.functions {
                        let method = &function.name;
                        let spelling = crate::javascript_name(method);
                        if let Some(previous) = methods.insert(spelling.clone(), method) {
                            return Err(Error(format!(
                                "{contract}: ambiguous JavaScript method {spelling}: {previous} and {method}"
                            )));
                        }
                    }
                    if *key != contract {
                        return Err(Error(
                            "export aliases require an explicit service mapping".into(),
                        ));
                    }
                    if interface.resources
                        || !interface
                            .functions
                            .iter()
                            .all(|function| function.kind.freestanding())
                    {
                        return Err(Error(format!(
                            "{contract}: resource exports require a lifecycle adapter"
                        )));
                    }
                    if !interface.functions.is_empty() {
                        let mut service = Service::new(
                            service_name(provider, &contract),
                            interface
                                .functions
                                .iter()
                                .map(|function| function.name.clone()),
                        );
                        service.signature = Some(self.digest(interface)?);
                        services.push(service);
                    }
                }
                Item::Type(_) => {}
                Item::Function(_) => {
                    return Err(Error(
                        "world function exports need an explicit service mapping".into(),
                    ));
                }
            }
        }
        Ok(services)
    }

    /// Derive kernel metadata after selecting installed Orb and host providers explicitly.
    pub fn plan(
        &self,
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
        orb.services = self.services(name)?;
        let mut requirements = BTreeMap::<String, Requirement>::new();
        let mut host_imports = Vec::new();
        let mut used = std::collections::BTreeSet::new();
        for (binding, interface) in self.provided_imports()? {
            let contract = interface.contract.clone().ok_or_else(|| {
                Error("anonymous import needs an explicit provider mapping".into())
            })?;
            let selected = bindings
                .get(binding)
                .ok_or_else(|| Error(format!("missing provider for {binding}")))?;
            used.insert(binding.to_owned());
            match selected {
                Binding::Host { provider } => {
                    if provider.trim().is_empty() {
                        return Err(Error(format!("empty host provider for {binding}")));
                    }
                    host_imports.push(HostImport {
                        binding: binding.to_owned(),
                        contract,
                        provider: provider.clone(),
                    });
                }
                Binding::Orb { name, api_version } => {
                    if name.trim().is_empty() || *api_version == 0 {
                        return Err(Error(format!("invalid Orb provider for {binding}")));
                    }
                    let service = service_name(name, &contract);
                    let requirement =
                        requirements
                            .entry(name.clone())
                            .or_insert_with(|| Requirement {
                                signatures: Default::default(),
                                hook_signatures: Default::default(),
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
                        .insert(service.clone(), self.digest(interface)?);
                    requirement.methods.insert(
                        service,
                        interface
                            .functions
                            .iter()
                            .map(|function| function.name.clone())
                            .collect(),
                    );
                }
            }
        }
        if let Some(unused) = bindings.keys().find(|name| !used.contains(*name)) {
            return Err(Error(format!("unused provider binding {unused}")));
        }
        orb.requires = requirements.into_values().collect();
        Ok(Plan { orb, host_imports })
    }

    /// Imports that need a provider: interfaces with functions or resources, in world order.
    pub(crate) fn provided_imports(&self) -> Result<Vec<(&str, &Interface)>, Error> {
        let mut imports = Vec::new();
        for Entry { key, item } in &self.model().imports {
            match item {
                Item::Interface(interface) if interface.callable() => {
                    imports.push((key.as_str(), interface));
                }
                Item::Interface(_) | Item::Type(_) => {}
                Item::Function(_) => {
                    return Err(Error(
                        "world function imports need an explicit provider mapping".into(),
                    ));
                }
            }
        }
        Ok(imports)
    }
}
