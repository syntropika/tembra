//! Hook roles reference real WIT callbacks; values and provider selection retain their existing owners.
use crate::{Binding, Error, Implementation, Item, Kind, PreparedContract, Ty, javascript_name};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookComposition {
    Series,
    Waterfall,
    Bail,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookBinding {
    pub name: String,
    pub alias: String,
    pub method: String,
    pub slot: u32,
    pub signature: String,
    pub composition: HookComposition,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContributionBinding {
    pub binding: String,
    pub target: String,
    pub name: String,
    pub alias: String,
    pub method: String,
    pub slot: u32,
    pub signature: String,
    pub priority: i32,
    pub optional: bool,
}

#[derive(Default, Debug, Serialize)]
pub struct HookPlan {
    pub hooks: Vec<HookBinding>,
    pub contributions: Vec<ContributionBinding>,
}

impl HookPlan {
    /// Hook callbacks are extension operations, not ordinary services or service dependencies.
    pub fn apply(&self, plan: &mut crate::Plan) {
        let exports: BTreeSet<_> = self
            .hooks
            .iter()
            .map(|hook| hook.name.as_str())
            .chain(self.contributions.iter().map(|hook| hook.name.as_str()))
            .collect();
        let prefix = format!("{}::", plan.orb.name);
        plan.orb.services.retain(|service| {
            !service
                .name
                .strip_prefix(&prefix)
                .is_some_and(|name| exports.contains(name))
        });
        for hook in &self.hooks {
            plan.orb.hooks.insert(hook.name.clone());
            plan.orb
                .hook_signatures
                .insert(hook.name.clone(), hook.signature.clone());
        }
        for contribution in &self.contributions {
            if let Some(requirement) = plan
                .orb
                .requires
                .iter_mut()
                .find(|item| item.orb == contribution.target)
            {
                let service = crate::service_name(&contribution.target, &contribution.name);
                requirement.methods.remove(&service);
                requirement.signatures.remove(&service);
                requirement.hooks.insert(contribution.name.clone());
                requirement
                    .hook_signatures
                    .insert(contribution.name.clone(), contribution.signature.clone());
            }
        }
    }
}

impl PreparedContract {
    /// Validate persisted contribution roles without relying on deployment provider names.
    pub(crate) fn check_contribution_roles(
        &self,
        contributions: &BTreeMap<String, i32>,
    ) -> Result<(), Error> {
        self.hook_exports(&BTreeMap::new(), contributions)?;
        for binding in contributions.keys() {
            let interface = self
                .model()
                .exports
                .iter()
                .find_map(|entry| match &entry.item {
                    Item::Interface(interface)
                        if interface.contract.as_deref() == Some(binding) =>
                    {
                        Some(interface)
                    }
                    _ => None,
                })
                .ok_or_else(|| Error(format!("missing contribution callback {binding}")))?;
            let function = &interface.functions[0];
            if self.function_signature(binding, &function.name, true)?
                != self.function_signature(binding, &function.name, false)?
            {
                return Err(Error(format!(
                    "{binding}: contribution callback differs from the required hook"
                )));
            }
        }
        Ok(())
    }

    /// Every hook interface has one callback. Roles are explicit; its input and output come from WIT.
    pub(crate) fn hook_interface(&self, alias: &str) -> Result<(&str, &crate::Function), Error> {
        let mut matches = self
            .model()
            .exports
            .iter()
            .filter_map(|entry| match &entry.item {
                Item::Interface(interface)
                    if javascript_name(interface.name.as_deref().unwrap_or_default()) == alias =>
                {
                    Some(interface)
                }
                _ => None,
            });
        let interface = matches
            .next()
            .ok_or_else(|| Error(format!("unknown hook export {alias}")))?;
        if matches.next().is_some() {
            return Err(Error(format!("ambiguous hook export alias {alias}")));
        }
        if !interface.resources.is_empty() || interface.functions.len() != 1 {
            return Err(Error(format!(
                "{alias}: a hook requires an interface with one plain callback"
            )));
        }
        let function = &interface.functions[0];
        if !function.kind.freestanding() || function.params.len() != 1 {
            return Err(Error(format!(
                "{alias}: a hook callback requires one input parameter"
            )));
        }
        Ok((
            interface
                .contract
                .as_deref()
                .ok_or_else(|| Error("hook interface requires an identity".into()))?,
            function,
        ))
    }

    pub(crate) fn hook_exports(
        &self,
        hooks: &BTreeMap<String, HookComposition>,
        contributions: &BTreeMap<String, i32>,
    ) -> Result<BTreeSet<String>, Error> {
        let mut exports = BTreeSet::new();
        for alias in hooks.keys() {
            exports.insert(self.hook_interface(alias)?.0.to_owned());
        }
        for binding in contributions.keys() {
            let interface = self
                .model()
                .exports
                .iter()
                .find_map(|entry| match &entry.item {
                    Item::Interface(interface)
                        if interface.contract.as_deref() == Some(binding) =>
                    {
                        Some(interface)
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    Error(format!(
                        "{binding}: a contribution must export its WIT callback interface"
                    ))
                })?;
            let alias = javascript_name(interface.name.as_deref().unwrap_or_default());
            let (identity, _) = self.hook_interface(&alias)?;
            if !exports.insert(identity.to_owned()) {
                return Err(Error(format!(
                    "{identity}: a callback cannot declare multiple hook roles"
                )));
            }
        }
        Ok(exports)
    }

    fn optional(&self, mut ty: Ty) -> bool {
        loop {
            let Ty::Ref(id) = ty else { return false };
            match &self.model().types[id as usize] {
                Kind::Alias(next) => ty = *next,
                Kind::Option(_) => return true,
                _ => return false,
            }
        }
    }

    /// Check role metadata against both WIT sides and the explicitly selected target provider.
    pub fn check_hooks(
        &self,
        implementation: &Implementation,
        bindings: &BTreeMap<String, Binding>,
    ) -> Result<HookPlan, Error> {
        self.hook_exports(&implementation.hooks, &implementation.contributes)?;
        self.check_contribution_roles(&implementation.contributes)?;
        let mut plan = HookPlan::default();
        for (alias, composition) in &implementation.hooks {
            let (name, function) = self.hook_interface(alias)?;
            match composition {
                HookComposition::Series if function.result.is_some() => {
                    return Err(Error(format!(
                        "{name}: series callbacks must return no value"
                    )));
                }
                HookComposition::Bail if !function.result.is_some_and(|ty| self.optional(ty)) => {
                    return Err(Error(format!(
                        "{name}: bail callbacks must return an option"
                    )));
                }
                _ => {}
            }
            if *composition == HookComposition::Waterfall {
                // Compare normalized complete type shapes, not resolver-local type IDs.
                let mut echo = self.model().clone();
                for entry in &mut echo.exports {
                    if let Item::Interface(interface) = &mut entry.item
                        && interface.contract.as_deref() == Some(name)
                    {
                        interface.functions[0].result = Some(interface.functions[0].params[0].ty);
                    }
                }
                let echo = PreparedContract::new(echo)?;
                if echo.function_signature(name, &function.name, true)?
                    != self.function_signature(name, &function.name, true)?
                {
                    return Err(Error(format!(
                        "{name}: waterfall input and result must have the same WIT type"
                    )));
                }
            }
            plan.hooks.push(HookBinding {
                name: name.into(),
                alias: alias.clone(),
                method: javascript_name(&function.name),
                slot: self.bind(name, &function.name, true)?,
                signature: self.function_signature(name, &function.name, true)?,
                composition: *composition,
            });
        }
        for (binding, priority) in &implementation.contributes {
            let Some(Binding::Orb { name: target, .. }) = bindings.get(binding) else {
                return Err(Error(format!(
                    "{binding}: a contribution requires a selected Orb provider"
                )));
            };
            let interface = self
                .model()
                .exports
                .iter()
                .find_map(|entry| match &entry.item {
                    Item::Interface(interface)
                        if interface.contract.as_deref() == Some(binding) =>
                    {
                        Some(interface)
                    }
                    _ => None,
                })
                .ok_or_else(|| Error(format!("missing contribution callback {binding}")))?;
            let alias = javascript_name(interface.name.as_deref().unwrap_or_default());
            let (name, function) = self.hook_interface(&alias)?;
            let signature = self.function_signature(name, &function.name, true)?;
            plan.contributions.push(ContributionBinding {
                binding: binding.clone(),
                target: target.clone(),
                name: name.into(),
                alias,
                method: javascript_name(&function.name),
                slot: self.bind(name, &function.name, true)?,
                signature,
                priority: *priority,
                optional: function.result.is_some_and(|ty| self.optional(ty)),
            });
        }
        Ok(plan)
    }
}

#[cfg(all(test, feature = "authoring"))]
mod tests {
    use super::*;
    use crate::Contract;

    const WIT: &str = "package example:hooks@1.0.0; interface guard { record admission { value: s32, blocked: bool } apply: func(input: admission) -> admission; } world provider { export guard; } world extension { import guard; export guard; }";

    #[test]
    fn hooks_never_select_the_first_of_ambiguous_export_aliases() {
        let contract = Contract::from_source("aliases.wit", "package example:hooks; interface first { apply: func(value: u32) -> u32; } interface second { apply: func(value: u32) -> u32; } world provider { export first; export second; }", None).unwrap();
        let mut model = contract.model().clone();
        for entry in &mut model.exports {
            if let Item::Interface(interface) = &mut entry.item {
                interface.name = Some("guard".into());
            }
        }
        let prepared = PreparedContract::new(model).unwrap();
        let implementation = Implementation {
            hooks: [("guard".into(), HookComposition::Waterfall)].into(),
            ..Default::default()
        };
        let error = prepared
            .check_hooks(&implementation, &BTreeMap::new())
            .unwrap_err();
        assert!(error.0.contains("ambiguous hook export alias"));
    }

    #[test]
    fn contribution_contracts_derive_hook_requirements_and_exclude_service_exports() {
        let contract = Contract::from_source("hooks.wit", WIT, Some("extension")).unwrap();
        let binding = "example:hooks/guard@1.0.0";
        let implementation = Implementation {
            contributes: [(binding.into(), 7)].into(),
            ..Default::default()
        };
        let bindings = [(
            binding.into(),
            Binding::Orb {
                name: "tools".into(),
                api_version: 1,
            },
        )]
        .into();
        assert!(
            contract
                .check_implementation("guard", 1, implementation.clone())
                .unwrap()
                .is_empty()
        );
        let checked = contract.check_hooks(&implementation, &bindings).unwrap();
        let mut plan = contract.plan("guard", 1, &bindings).unwrap();
        checked.apply(&mut plan);
        assert!(plan.orb.services.is_empty());
        assert!(plan.orb.requires[0].methods.is_empty());
        assert_eq!(plan.orb.requires[0].hooks, [binding.into()].into());
        assert_eq!(checked.contributions[0].priority, 7);
        let provider = Contract::from_source("hooks.wit", WIT, Some("provider")).unwrap();
        assert_eq!(
            checked.contributions[0].signature,
            provider.function_signature(binding, "apply", true).unwrap()
        );
    }

    #[test]
    fn hook_compositions_are_checked_against_real_callback_types() {
        let contract = Contract::from_source("hooks.wit", WIT, Some("provider")).unwrap();
        let mut implementation = Implementation {
            hooks: [("guard".into(), HookComposition::Waterfall)].into(),
            ..Default::default()
        };
        let checked = contract
            .check_hooks(&implementation, &BTreeMap::new())
            .unwrap();
        let mut plan = contract.plan("tools", 1, &BTreeMap::new()).unwrap();
        checked.apply(&mut plan);
        assert!(plan.orb.services.is_empty());
        assert_eq!(plan.orb.hook_signatures.len(), 1);
        for invalid in [HookComposition::Series, HookComposition::Bail] {
            implementation.hooks.insert("guard".into(), invalid);
            assert!(
                contract
                    .check_hooks(&implementation, &BTreeMap::new())
                    .is_err()
            );
        }
    }
}
