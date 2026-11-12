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
    /// Validate the hooks a compiled component declares: each names an exported interface with
    /// one callback whose shape fits the composition.
    pub(crate) fn check_hook_roles(
        &self,
        hooks: &BTreeMap<String, HookComposition>,
    ) -> Result<(), Error> {
        for (identity, composition) in hooks {
            let alias = self
                .model()
                .exports
                .iter()
                .find_map(|entry| match &entry.item {
                    Item::Interface(interface)
                        if interface.contract.as_deref() == Some(identity) =>
                    {
                        Some(javascript_name(
                            interface.name.as_deref().unwrap_or_default(),
                        ))
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    Error(format!(
                        "{identity}: a declared hook must export its WIT callback interface"
                    ))
                })?;
            let (name, function) = self.hook_interface(&alias)?;
            self.check_composition(name, function, *composition)?;
        }
        Ok(())
    }

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
    fn hook_interface(&self, alias: &str) -> Result<(&str, &crate::Function), Error> {
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

    /// Whether the callback's shape fits the composition: series callbacks return nothing, bail
    /// callbacks return an option, waterfall callbacks return their input type.
    fn check_composition(
        &self,
        name: &str,
        function: &crate::Function,
        composition: HookComposition,
    ) -> Result<(), Error> {
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
        if composition == HookComposition::Waterfall {
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
        Ok(())
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
        let mut declared = BTreeSet::new();
        for (alias, composition) in &implementation.hooks {
            let (name, function) = self.hook_interface(alias)?;
            self.check_composition(name, function, *composition)?;
            declared.insert(name);
            // A compiled provider imports its own hook to run it; that import is the kernel's
            // plan, never a provider's service, so a requirement for it is a mistake. Only a
            // callable import binds: an interface the world reaches only for its types (another
            // import `use`s them) is no entry to the plan and receives no binding at all.
            let imported = self
                .model()
                .imports
                .iter()
                .find_map(|entry| match &entry.item {
                    Item::Interface(interface)
                        if interface.callable() && interface.contract.as_deref() == Some(name) =>
                    {
                        Some(entry.key.as_str())
                    }
                    _ => None,
                });
            if let Some(binding) = imported
                && !matches!(bindings.get(binding), Some(Binding::Hook))
            {
                return Err(Error(format!(
                    "{name}: the import of a declared hook runs its plan; it binds to no provider"
                )));
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
        // `providers` binds to the kernel's plan every import the world also exports and no
        // requirement selects; only an Orb that declares that hook runs it. Any other such import
        // has no provider, which installation refuses here instead of trapping on the first call.
        for (binding, selected) in bindings {
            if !matches!(selected, Binding::Hook) {
                continue;
            }
            let identity = self
                .model()
                .imports
                .iter()
                .find_map(|entry| match &entry.item {
                    Item::Interface(interface) if entry.key == *binding => {
                        interface.contract.as_deref()
                    }
                    _ => None,
                });
            if !identity.is_some_and(|identity| declared.contains(identity)) {
                return Err(Error(format!(
                    "{binding}: the world imports an interface it also exports, but the Orb declares no hook for it; select an Orb provider for the import or declare the hook"
                )));
            }
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

    /// A compiled provider imports the hook it exports, to run its plan: without a requirement
    /// the import binds to the plan, the lowering admits it, and a provider binding on it is
    /// refused because it would route the run to another Orb's service.
    #[test]
    fn a_provider_imports_its_own_hook_as_the_kernel_plan() {
        let contract = Contract::from_source("hooks.wit", "package example:hooks@1.0.0; interface guard { record admission { value: s32, blocked: bool } apply: func(input: admission) -> admission; } interface gate { use guard.{admission}; admit: func(value: s32) -> admission; } world provider { import guard; export guard; export gate; }", Some("provider")).unwrap();
        let binding = "example:hooks/guard@1.0.0";
        let bindings = contract
            .providers(&BTreeMap::new(), &crate::Grants::default())
            .unwrap();
        assert_eq!(bindings, [(binding.into(), Binding::Hook)].into());
        let mut plan = contract.plan("gate", 1, &bindings).unwrap();
        assert!(plan.orb.requires.is_empty() && plan.host_imports.is_empty());
        let implementation = Implementation {
            hooks: [("guard".into(), HookComposition::Waterfall)].into(),
            ..Default::default()
        };
        let checked = contract.check_hooks(&implementation, &bindings).unwrap();
        assert_eq!(checked.hooks[0].name, binding);
        checked.apply(&mut plan);
        assert_eq!(plan.orb.hooks, [binding.into()].into());
        assert_eq!(plan.orb.services.len(), 1, "gate stays, guard is the hook");
        let provider = [(
            binding.into(),
            Binding::Orb {
                name: "other".into(),
                api_version: 1,
            },
        )]
        .into();
        assert!(contract.check_hooks(&implementation, &provider).is_err());
        // Without the export the import is an ordinary dependency and needs a requirement.
        let consumer = Contract::from_source("hooks.wit", "package example:hooks@1.0.0; interface guard { record admission { value: s32, blocked: bool } apply: func(input: admission) -> admission; } world consumer { import guard; }", Some("consumer")).unwrap();
        assert!(
            consumer
                .providers(&BTreeMap::new(), &crate::Grants::default())
                .is_err()
        );
    }

    /// An import the world also exports binds to the kernel's plan only for a hook the Orb
    /// declares; without that declaration nothing would ever run it, so installation refuses it
    /// rather than letting the first call trap.
    #[test]
    fn a_hook_import_without_a_declared_hook_is_refused() {
        let contract = Contract::from_source("hooks.wit", "package example:hooks@1.0.0; interface guard { record admission { value: s32, blocked: bool } apply: func(input: admission) -> admission; } interface gate { use guard.{admission}; admit: func(value: s32) -> admission; } world provider { import guard; export guard; export gate; }", Some("provider")).unwrap();
        let bindings = contract
            .providers(&BTreeMap::new(), &crate::Grants::default())
            .unwrap();
        assert_eq!(
            bindings,
            [("example:hooks/guard@1.0.0".into(), Binding::Hook)].into()
        );
        let error = contract
            .check_hooks(&Implementation::default(), &bindings)
            .unwrap_err();
        assert!(error.0.contains("declares no hook for it"), "{error:?}");
    }

    /// An interface the world reaches only because another import `use`s its types is an
    /// implicit import: it is not callable, `providers` binds nothing to it, and declaring the
    /// hook it defines needs no binding for it either.
    #[test]
    fn a_declared_hook_reached_only_for_its_types_needs_no_binding() {
        let contract = Contract::from_source("gate.wit", "package example:gate@1.0.0; interface guard { record admission { value: s32, blocked: bool } apply: func(input: admission) -> admission; } interface policy { use guard.{admission}; check: func(value: s32) -> admission; } interface runner { admit: func(value: s32) -> s32; } world gate { import policy; export guard; export runner; }", Some("gate")).unwrap();
        let requires = [(
            "example:gate/policy@1.0.0".to_owned(),
            crate::OrbProvider {
                orb: "policy".into(),
                api_version: 1,
            },
        )]
        .into();
        let bindings = contract
            .providers(&requires, &crate::Grants::default())
            .unwrap();
        assert!(!bindings.contains_key("example:gate/guard@1.0.0"));
        let implementation = Implementation {
            hooks: [("guard".into(), HookComposition::Waterfall)].into(),
            ..Default::default()
        };
        let checked = contract.check_hooks(&implementation, &bindings).unwrap();
        assert_eq!(checked.hooks[0].name, "example:gate/guard@1.0.0");
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
