//! Resolve deployment choices without making a WIT import an implicit capability grant.
use crate::{Binding, Contract, Grants, describe::has_resource};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tembra_core::{Code, Error, Result};
use wit_parser::WorldItem;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrbProvider {
    pub orb: String,
    pub api_version: u32,
}

impl Contract {
    /// Derive host bindings from grants and Orb bindings from the installation's explicit choices.
    pub fn providers(
        &self,
        requires: &BTreeMap<String, OrbProvider>,
        grants: &Grants,
    ) -> Result<BTreeMap<String, Binding>> {
        let mut bindings = BTreeMap::new();
        let mut used = BTreeSet::new();
        for (key, item) in &self.resolve.worlds[self.world].imports {
            let WorldItem::Interface { id, .. } = item else {
                continue;
            };
            let interface = &self.resolve.interfaces[*id];
            if interface.functions.is_empty() && !has_resource(&self.resolve, *id) {
                continue;
            }
            let binding = self.resolve.name_world_key(key);
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
            let contract = self.resolve.id_of(*id).ok_or_else(|| {
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
            } else if contract == "wasi:clocks/system-clock@0.3.0" {
                bindings.insert(
                    binding,
                    Binding::Host {
                        provider: "system-clock".into(),
                    },
                );
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
