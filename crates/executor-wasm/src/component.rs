//! Compiled-language components share the source Orb session, port and lifecycle adapters.
//!
//! A component cannot describe itself at runtime, so its implementation description is derived
//! from the prepared WIT contract and checked by the same `check_implementation` as a source
//! bundle. Its Store then serves the same private host protocol as the interpreter.
mod values;
pub(crate) mod worker;
use crate::{Binding, Engine, Grants, failure, worker::Session};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, sync::mpsc};
use tembra::Result;
use tembra_contracts::{
    FunctionKind, Item, Plan, PreparedContract, ServiceBinding, javascript_name,
};

/// Describe the plain interface exports a compiled artifact implements.
///
/// Compiled services have no guest-selected lifetime, linger or access policy.
pub(crate) fn describe(
    contract: &PreparedContract,
    name: &str,
    api_version: u32,
    contributions: &BTreeMap<String, i32>,
) -> Value {
    let mut services = Map::new();
    for entry in &contract.model().exports {
        let Item::Interface(interface) = &entry.item else {
            continue;
        };
        if interface.contract.is_none() || interface.functions.is_empty() {
            continue;
        }
        if interface
            .contract
            .as_ref()
            .is_some_and(|identity| contributions.contains_key(identity))
        {
            continue;
        }
        let methods = interface
            .functions
            .iter()
            .map(|function| javascript_name(&function.name))
            .collect::<Vec<_>>();
        services.insert(
            javascript_name(interface.name.as_deref().unwrap_or_default()),
            json!({ "methods": methods }),
        );
    }
    json!({ "name": name, "apiVersion": api_version, "services": services, "contributes": contributions })
}

/// Resolve checked exports and selected Orb imports into Send-only Store dispatch tables.
fn layout(
    contract: &PreparedContract,
    services: &[ServiceBinding],
    bindings: &BTreeMap<String, Binding>,
    hooks: &tembra_contracts::HookPlan,
) -> Result<worker::Layout> {
    let mut exports: BTreeMap<_, _> = services
        .iter()
        .map(|service| {
            (
                service.alias.clone(),
                worker::Export {
                    interface: service.interface.clone(),
                    methods: service
                        .methods
                        .iter()
                        .map(|(method, spelling)| (spelling.clone(), method.clone()))
                        .collect(),
                },
            )
        })
        .collect();
    let mut contributions = BTreeMap::new();
    for contribution in &hooks.contributions {
        let function = contract
            .model()
            .exports
            .iter()
            .find_map(|entry| match &entry.item {
                Item::Interface(interface)
                    if interface.contract.as_deref() == Some(&contribution.name) =>
                {
                    interface.functions.first()
                }
                _ => None,
            })
            .ok_or_else(|| failure("checked contribution callback is missing"))?;
        exports.insert(
            contribution.alias.clone(),
            worker::Export {
                interface: contribution.name.clone(),
                methods: [(contribution.method.clone(), function.name.clone())].into(),
            },
        );
        contributions.insert(
            contribution.binding.clone(),
            (contribution.alias.clone(), contribution.method.clone()),
        );
    }
    let mut imports = Vec::new();
    for entry in &contract.model().imports {
        if !matches!(bindings.get(&entry.key), Some(Binding::Orb { .. })) {
            continue;
        }
        let Item::Interface(interface) = &entry.item else {
            return Err(failure("Orb import must be an interface"));
        };
        if interface.resources {
            return Err(failure(format!(
                "{}: resource imports require a lifecycle adapter",
                entry.key
            )));
        }
        let functions = interface
            .functions
            .iter()
            .map(|function| match function.kind {
                FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => {
                    Ok(worker::ImportFunction {
                        name: function.name.clone(),
                        spelling: javascript_name(&function.name),
                        asynchronous: function.kind == FunctionKind::AsyncFreestanding,
                    })
                }
                _ => Err(failure(format!(
                    "{}#{}: only freestanding Orb import functions are supported",
                    entry.key, function.name
                ))),
            })
            .collect::<Result<Vec<_>>>()?;
        imports.push(worker::Import {
            binding: entry.key.clone(),
            functions,
            contribution: contributions.contains_key(&entry.key),
        });
    }
    Ok(worker::Layout {
        exports,
        imports,
        contributions,
    })
}

impl Engine {
    /// Start a compiled Store whose WASI context holds only this installation's grants.
    ///
    /// Instantiation completes before this returns, so link and initialization failures are
    /// reported at installation rather than on the first call.
    pub(crate) fn component_session(
        &self,
        artifact: &tembra_contracts::Artifact,
        plan: &Plan,
        services: &[ServiceBinding],
        bindings: &BTreeMap<String, Binding>,
        grants: &Grants,
        hooks: &tembra_contracts::HookPlan,
    ) -> Result<Session> {
        let tembra_contracts::ArtifactImplementation::Component(bytes) = &artifact.implementation
        else {
            return Err(failure("compiled execution requires a component artifact"));
        };
        let layout = layout(&artifact.contract, services, bindings, hooks)?;
        let wasi = self.providers.context(&plan.host_imports, grants)?;
        let component = self.compiled(bytes)?.as_ref().clone();
        // Compiled Orbs use their WASI preopens and sockets; the source I/O helpers stay empty.
        let io = crate::io::SourceIo::new(
            &self.providers,
            &[],
            &Grants::default(),
            std::time::Duration::from_millis(self.limits.timeout_ms),
        )?;
        let (ready, initialized) = mpsc::channel();
        let engine = self.clone();
        let session = Session::spawn(
            self,
            artifact.identity.name.clone(),
            io,
            move |commands, _poison, _diagnostics| {
                worker::run(engine, component, wasi, layout, ready, commands)
            },
        )?;
        match initialized.recv() {
            Ok(Ok(())) => Ok(session),
            Ok(Err(error)) => Err(failure(error)),
            Err(_) => Err(failure("component worker stopped during initialization")),
        }
    }
}
