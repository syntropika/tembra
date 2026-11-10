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

/// Require grants for every standard WASI instance the actual binary imports with at least one
/// function, before instantiation. Artifact metadata (including type-only dependency claims)
/// is not trusted here: only Wasmtime's own type information of the compiled component is.
/// Instances exporting only types or resources need no grant. Non-WASI imports are linked
/// separately from the checked Orb layout.
pub(crate) fn check_component_imports(
    engine: &wasmtime::Engine,
    component: &wasmtime::component::Component,
    grants: &Grants,
) -> Result<()> {
    use wasmtime::component::types::ComponentItem;
    for (name, item) in component.component_type().imports(engine) {
        let ComponentItem::ComponentInstance(instance) = item.ty else {
            continue;
        };
        if !name.starts_with("wasi:") {
            continue;
        }
        if instance
            .exports(engine)
            .any(|(_, item)| matches!(item.ty, ComponentItem::ComponentFunc(_)))
        {
            grants.check_component_import(name)?;
        }
    }
    Ok(())
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
        (grants, cli): (&Grants, &crate::Cli),
        hooks: &tembra_contracts::HookPlan,
    ) -> Result<Session> {
        let tembra_contracts::ArtifactImplementation::Component(bytes) = &artifact.implementation
        else {
            return Err(failure("compiled execution requires a component artifact"));
        };
        let layout = layout(&artifact.contract, services, bindings, hooks)?;
        // Admit the binary's actual imports before preparing any provider: a refused
        // installation opens no CLI file and invokes no configured factory.
        let component = self.compiled(bytes)?.as_ref().clone();
        check_component_imports(&self.engine, &component, grants)?;
        let wasi = self
            .providers
            .context(&plan.host_imports, grants, cli)?
            .wasi;
        // Compiled Orbs use their WASI preopens and sockets; the source I/O helpers stay empty.
        let io = crate::io::SourceIo::new(
            &self.providers,
            &[],
            &Grants::default(),
            std::time::Duration::from_millis(self.limits.timeout_ms),
            &crate::Cli::default(),
        )?;
        let (ready, initialized) = mpsc::channel();
        let engine = self.clone();
        let grants = std::sync::Arc::new(grants.clone());
        let session = Session::spawn(
            self,
            artifact.identity.name.clone(),
            io,
            move |commands, _poison, _diagnostics| {
                worker::run(engine, component, wasi, grants, layout, ready, commands)
            },
        )?;
        match initialized.recv() {
            Ok(Ok(())) => Ok(session),
            Ok(Err(error)) => Err(failure(error)),
            Err(_) => Err(failure("component worker stopped during initialization")),
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::check_component_imports;
    use crate::{FilesystemGrant, Grants, SystemCapability};

    /// A clock instance exporting only its `instant` type, and a filesystem instance whose
    /// function returns that type: the shape of a component that calls `stat` but never a clock.
    const STAT_SHAPED: &str = r#"(component
      (import "wasi:clocks/system-clock@0.3.0" (instance $clock
        (type $instant (record (field "seconds" s64) (field "nanoseconds" u32)))
        (export "instant" (type (eq $instant)))))
      (alias export $clock "instant" (type $instant))
      (import "wasi:filesystem/types@0.3.0" (instance
        (alias outer 1 $instant (type $t))
        (export "instant" (type $u (eq $t)))
        (export "modified" (func (result $u))))))"#;

    /// A component that can actually call the system clock.
    const CLOCK_CALL: &str = r#"(component
      (import "wasi:clocks/system-clock@0.3.0" (instance
        (export "get-resolution" (func (result u64))))))"#;

    /// A custom (non-standard) import is linked from the Orb layout, never as a builtin.
    const CUSTOM: &str = r#"(component
      (import "example:clock/wall@1.0.0" (instance
        (export "now" (func (result u64))))))"#;

    fn grants(clocks: bool, filesystem: bool) -> Grants {
        Grants {
            filesystem: filesystem
                .then(|| FilesystemGrant {
                    provider: "data".into(),
                    mount: "/data".into(),
                    writable: false,
                })
                .into_iter()
                .collect(),
            system: if clocks {
                [SystemCapability::Clocks].into()
            } else {
                Default::default()
            },
            ..Grants::default()
        }
    }

    fn check(wat: &str, grants: &Grants) -> tembra::Result<()> {
        let engine = crate::Engine::new(crate::Limits::default()).unwrap();
        let bytes = wat::parse_str(wat).unwrap();
        let component = wasmtime::component::Component::new(&engine.engine, bytes).unwrap();
        check_component_imports(&engine.engine, &component, grants)
    }

    #[test]
    fn actual_binary_clock_calls_need_the_grant_before_instantiation() {
        let refused = check(CLOCK_CALL, &grants(false, true)).unwrap_err();
        assert!(refused.to_string().contains("system-clock"), "{refused}");
        check(CLOCK_CALL, &grants(true, false)).unwrap();
    }

    #[test]
    fn clock_types_used_by_stat_need_no_clock_grant() {
        check(STAT_SHAPED, &grants(false, true)).unwrap();
        // The filesystem instance itself is callable and still needs its grant.
        assert!(check(STAT_SHAPED, &grants(true, false)).is_err());
    }

    #[test]
    fn custom_namespaces_are_never_builtin_permissions() {
        check(CUSTOM, &Grants::default()).unwrap();
    }

    /// The real compiled filesystem probe imports no clock at all (PHASE3_IMPORT_PROOF.md).
    #[test]
    #[ignore = "requires TEMBRA_WASM_PROBE"]
    fn compiled_filesystem_probe_needs_no_clock_grant() {
        let path = std::env::var_os("TEMBRA_WASM_PROBE").expect("TEMBRA_WASM_PROBE");
        let engine = crate::Engine::new(crate::Limits::default()).unwrap();
        let component = wasmtime::component::Component::from_file(&engine.engine, path).unwrap();
        check_component_imports(&engine.engine, &component, &grants(false, true)).unwrap();
        assert!(check_component_imports(&engine.engine, &component, &grants(true, false)).is_err());
    }
}
