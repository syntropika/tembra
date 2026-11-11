//! What a compiled component carries in its artifact beyond its contract.
//!
//! A source bundle declares its hook roles and its services' lifetimes in executable code, which
//! the executor reads when it loads the bundle. A component cannot describe itself at run time, so
//! its artifact carries a manifest next to the contract, and the executor derives the description
//! from both. Every entry is keyed by a versioned interface identity of the contract and checked
//! against it when the artifact is packaged and again when it is decoded.
use crate::{Error, HookComposition, Item, Lifetime, PreparedContract, guest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The manifest of a compiled component: the hooks it contributes to, with their priorities, the
/// hooks it declares, with their composition, and the ownership policy of its services. An
/// interface takes one hook role, and a service is never a hook callback.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub contributions: BTreeMap<String, i32>,
    pub hooks: BTreeMap<String, HookComposition>,
    pub services: BTreeMap<String, ServicePolicy>,
}

/// The ownership policy of one service's objects, which the kernel applies: their lifetime and,
/// for referenced objects, how long an unreferenced one lingers before the kernel releases it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServicePolicy {
    pub lifetime: Lifetime,
    /// Idle retention of an unreferenced object, in milliseconds. A host-lifetime object never
    /// lingers: it lives until the host shuts down.
    pub linger_ms: u32,
}

impl PreparedContract {
    /// Validate a manifest against this contract, without relying on deployment provider names.
    pub(crate) fn check_manifest(&self, manifest: &Manifest) -> Result<(), Error> {
        if let Some(both) = manifest
            .hooks
            .keys()
            .find(|identity| manifest.contributions.contains_key(*identity))
        {
            return Err(Error(format!(
                "{both}: an Orb declares a hook or contributes to it, not both"
            )));
        }
        self.check_contribution_roles(&manifest.contributions)?;
        self.check_hook_roles(&manifest.hooks)?;
        self.check_service_policies(manifest)
    }

    /// Each policy names an exported service interface: one with functions that is neither a
    /// hook callback, nor a kernel callback, nor the incoming HTTP handler. Host-lifetime objects
    /// cannot linger, as the kernel rules.
    fn check_service_policies(&self, manifest: &Manifest) -> Result<(), Error> {
        for (identity, policy) in &manifest.services {
            let exported = self.model().exports.iter().any(|entry| {
                !crate::lower::is_ingress(entry)
                    && matches!(&entry.item, Item::Interface(interface)
                        if interface.contract.as_deref() == Some(identity) && interface.callable())
            });
            if !exported
                || guest::reserved(identity)
                || manifest.hooks.contains_key(identity)
                || manifest.contributions.contains_key(identity)
            {
                return Err(Error(format!(
                    "{identity}: a service policy names an exported service interface"
                )));
            }
            if policy.lifetime == Lifetime::Host && policy.linger_ms != 0 {
                return Err(Error(format!(
                    "{identity}: host-lifetime objects cannot linger"
                )));
            }
        }
        Ok(())
    }
}
