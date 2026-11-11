//! Resolve deployment choices without making a WIT import an implicit capability grant.
use crate::{Binding, Grants, PreparedContract, guest, model::Item};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tembra_core::{Code, Error, Result};

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrbProvider {
    pub orb: String,
    pub api_version: u32,
}

impl PreparedContract {
    /// Derive host bindings from grants and Orb bindings from the installation's explicit choices.
    pub fn providers(
        &self,
        requires: &BTreeMap<String, OrbProvider>,
        grants: &Grants,
    ) -> Result<BTreeMap<String, Binding>> {
        let mut bindings = BTreeMap::new();
        let mut used = BTreeSet::new();
        for entry in &self.model().imports {
            let Item::Interface(interface) = &entry.item else {
                continue;
            };
            if !interface.callable() {
                continue;
            }
            let binding = entry.key.clone();
            // The kernel's own interfaces are neither Orb dependencies nor host capabilities.
            if let Some(contract) = interface
                .contract
                .as_deref()
                .filter(|id| guest::reserved(id))
            {
                if !guest::serves(contract) {
                    return Err(Error::new(
                        Code::Unsupported,
                        "wit_binding",
                        format!(
                            "{binding}: the tembra namespace is reserved for kernel interfaces; this kernel serves {}",
                            guest::IMPORTS.join(", ")
                        ),
                    ));
                }
                bindings.insert(binding, Binding::Kernel);
                continue;
            }
            if let Some(requirement) = requires.get(&binding) {
                if requirement.orb.trim().is_empty() || requirement.api_version == 0 {
                    return Err(Error::new(
                        Code::Invalid,
                        "wit_binding",
                        format!("{binding}: requires a provider name and positive apiVersion"),
                    ));
                }
                bindings.insert(
                    binding.clone(),
                    Binding::Orb {
                        name: requirement.orb.clone(),
                        api_version: requirement.api_version,
                    },
                );
                used.insert(binding);
                continue;
            }
            let contract = interface.contract.clone().ok_or_else(|| {
                Error::new(
                    Code::Unsupported,
                    "wit_binding",
                    "anonymous imports require an adapter",
                )
            })?;
            let providers = if contract.starts_with("wasi:filesystem/") {
                grants
                    .filesystem
                    .iter()
                    .map(|grant| grant.provider.as_str())
                    .collect::<BTreeSet<_>>()
            } else if contract.starts_with("wasi:sockets/") {
                grants
                    .network
                    .iter()
                    .map(|grant| grant.provider.as_str())
                    .collect()
            } else if contract.starts_with("wasi:http/") {
                // A world that only serves incoming HTTP still imports the request and response
                // types. Without an HTTP grant they bind to the ingress provider, which carries
                // no outgoing authority; `wasi:http/client` stays ungranted.
                if grants.http.is_empty()
                    && contract.starts_with("wasi:http/types@")
                    && self.ingress().is_some()
                {
                    bindings.insert(
                        binding,
                        Binding::Host {
                            provider: crate::lower::INGRESS_PROVIDER.into(),
                        },
                    );
                    continue;
                }
                grants
                    .http
                    .iter()
                    .map(|grant| grant.provider.as_str())
                    .collect()
            } else if let Some(capability) = crate::permissions::system_capability(&contract) {
                let provider = match capability {
                    crate::SystemCapability::Clocks => "clocks",
                    crate::SystemCapability::Random => "random",
                    crate::SystemCapability::Cli => "cli",
                };
                bindings.insert(
                    binding,
                    Binding::Host {
                        provider: provider.into(),
                    },
                );
                continue;
            } else if self.model().exports.iter().any(|entry| {
                matches!(&entry.item, Item::Interface(exported)
                    if exported.contract.as_deref() == Some(contract.as_str()))
            }) {
                // An import of an interface this world also exports, with no provider required
                // for it, is the Orb's typed entry to a hook it declares: the kernel runs its plan.
                bindings.insert(binding, Binding::Hook);
                continue;
            } else {
                return Err(Error::new(
                    Code::Invalid,
                    "wit_binding",
                    format!("{binding}: missing Orb provider requirement"),
                ));
            };
            if providers.is_empty() {
                return Err(Error::new(
                    Code::Forbidden,
                    "orb_grant",
                    format!("{binding}: no provider has been granted to this Orb"),
                ));
            }
            if providers.len() != 1 {
                return Err(Error::new(
                    Code::Invalid,
                    "wit_binding",
                    format!(
                        "{binding}: multiple granted providers require an explicit mount adapter"
                    ),
                ));
            }
            bindings.insert(
                binding,
                Binding::Host {
                    provider: (*providers.first().unwrap()).into(),
                },
            );
        }
        if let Some(unused) = requires.keys().find(|name| !used.contains(*name)) {
            return Err(Error::new(
                Code::Invalid,
                "wit_binding",
                format!("unused Orb requirement {unused}"),
            ));
        }
        Ok(bindings)
    }
}
