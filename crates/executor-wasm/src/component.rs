//! Compiled-language components share the source Orb session, port and lifecycle adapters.
//!
//! A component cannot describe itself at runtime, so its implementation description is derived
//! from the prepared WIT contract and checked by the same `check_implementation` as a source
//! bundle. Its Store then serves the same private host protocol as the interpreter.
pub(crate) mod resources;
mod values;
pub(crate) mod worker;
use crate::{Binding, Engine, Grants, failure, worker::Session};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, sync::mpsc};
use tembra::Result;
use tembra_contracts::{
    FunctionKind, Item, Plan, PreparedContract, ServiceBinding, javascript_name,
};
pub(crate) use worker::State as ComponentState;

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
        // The incoming HTTP handler is declared like a service so `check_ingress` consumes it;
        // it never becomes a portable service binding.
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
    let mut exports = BTreeMap::new();
    for service in services {
        let mut methods = BTreeMap::new();
        let mut roles = BTreeMap::new();
        for (method, spelling) in &service.methods {
            methods.insert(spelling.clone(), method.clone());
            let slot = contract
                .bind(&service.interface, method, true)
                .map_err(failure)?;
            let role = resource_roles(contract, slot)?;
            if role.receiver.is_some() || role.own_result.is_some() {
                roles.insert(spelling.clone(), role);
            }
        }
        exports.insert(
            service.alias.clone(),
            worker::Export {
                interface: service.interface.clone(),
                methods,
                roles,
            },
        );
    }
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
                roles: BTreeMap::new(),
            },
        );
        contributions.insert(
            contribution.binding.clone(),
            (contribution.alias.clone(), contribution.method.clone()),
        );
    }
    let mut imports = Vec::new();
    // One dynamic host type per defining resource: every alias of it, in any selected import,
    // links the same type, and distinct resources (even with equal names) never share one.
    let mut payloads = BTreeMap::<(String, String), u32>::new();
    for entry in &contract.model().imports {
        if !matches!(bindings.get(&entry.key), Some(Binding::Orb { .. })) {
            continue;
        }
        let Item::Interface(interface) = &entry.item else {
            return Err(failure("Orb import must be an interface"));
        };
        // `open` admitted these shapes: resource imports only through the capability transport.
        let mut resources = Vec::new();
        for (name, ty) in &interface.resources {
            let nominal = nominal(contract.resource_identity(tembra_contracts::Ty::Ref(*ty)))?;
            let next = u32::try_from(payloads.len() + 1).map_err(failure)?;
            let payload = *payloads.entry(nominal).or_insert(next);
            resources.push((name.clone(), payload));
        }
        let payload = |role: Option<&tembra_contracts::ResourceIdentity>| {
            role.map(|identity| {
                payloads
                    .get(&nominal(Some(identity))?)
                    .copied()
                    .ok_or_else(|| failure("imported resource is not linked by its interface"))
            })
            .transpose()
        };
        let functions = interface
            .functions
            .iter()
            .map(|function| {
                let asynchronous = match function.kind {
                    FunctionKind::Freestanding
                    | FunctionKind::Constructor(_)
                    | FunctionKind::Static(_)
                    | FunctionKind::Method(_) => false,
                    FunctionKind::AsyncFreestanding
                    | FunctionKind::AsyncStatic(_)
                    | FunctionKind::AsyncMethod(_) => true,
                    _ => {
                        return Err(failure(format!(
                            "{}#{}: resource getters and setters are not supported",
                            entry.key, function.name
                        )));
                    }
                };
                let slot = contract
                    .bind(&entry.key, &function.name, false)
                    .map_err(failure)?;
                let signature = contract.resource_signature(slot).map_err(failure)?;
                if signature.stream_result && !asynchronous {
                    return Err(failure(format!(
                        "{}#{}: a stream import must be asynchronous",
                        entry.key, function.name
                    )));
                }
                Ok(worker::ImportFunction {
                    name: function.name.clone(),
                    spelling: javascript_name(&function.name),
                    asynchronous,
                    receiver: payload(signature.receiver)?,
                    own_result: payload(signature.own_result)?,
                    stream: signature.stream_result,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        imports.push(worker::Import {
            binding: entry.key.clone(),
            resources,
            functions,
            contribution: contributions.contains_key(&entry.key),
        });
    }
    Ok(worker::Layout {
        exports,
        imports,
        contributions,
        ingress: contract.ingress().map(str::to_owned),
    })
}

/// The defining identity of a resource; an anonymous resource cannot cross between Orbs.
fn nominal(identity: Option<&tembra_contracts::ResourceIdentity>) -> Result<(String, String)> {
    match identity {
        Some(tembra_contracts::ResourceIdentity {
            interface: Some(interface),
            name: Some(name),
        }) => Ok((interface.clone(), name.clone())),
        _ => Err(failure(
            "an imported resource has no portable type identity",
        )),
    }
}

/// Resource positions of one checked function, by the local name of the defining resource.
fn resource_roles(contract: &PreparedContract, slot: u32) -> Result<worker::Roles> {
    let signature = contract.resource_signature(slot).map_err(failure)?;
    let name = |identity: Option<&tembra_contracts::ResourceIdentity>| {
        identity.and_then(|identity| identity.name.clone())
    };
    Ok(worker::Roles {
        receiver: name(signature.receiver),
        own_result: name(signature.own_result),
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
    // Request and response types are admitted without an outgoing grant only for a binary that
    // itself exports the incoming HTTP handler; the client never is.
    let serves_http = component
        .component_type()
        .exports(engine)
        .any(|(name, _)| name == tembra_contracts::HTTP_HANDLER);
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
            grants.check_component_import(name, serves_http)?;
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
        let prepared = self.providers.context(&plan.host_imports, grants, cli)?;
        // The WASI context and whether the bound name-lookup provider grants resolution.
        let wasi = (prepared.wasi, prepared.resolve);
        // Compiled Orbs use their WASI preopens and sockets; the source I/O helpers stay empty.
        let io = crate::io::SourceIo::new(
            &self.providers,
            &[],
            &Grants::default(),
            &crate::Cli::default(),
        )?;
        let (ready, initialized) = mpsc::channel();
        let scope = io.scope().clone();
        let engine = self.clone();
        let grants = std::sync::Arc::new(grants.clone());
        let session = Session::spawn(
            self,
            artifact.identity.name.clone(),
            io,
            move |commands, poison, _diagnostics| {
                worker::run(
                    engine,
                    component,
                    wasi,
                    grants,
                    layout,
                    ready,
                    (commands, scope, poison),
                )
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

#[cfg(test)]
mod resource_link_tests {
    use super::layout;
    use crate::{Binding, Contract, Limits};
    use std::collections::BTreeMap;
    use wasmtime::component::{Component, Linker, ResourceType};

    const SESSIONS: &str = "tembra-test:resources/sessions@1.0.0";
    const VIEWS: &str = "tembra-test:resources/views@1.0.0";

    /// An importer that retains `views`' alias of `sessions`' resource, as a guest compiler
    /// may; the fixture binary itself elides the unused alias.
    const ALIASING: &str = r#"(component
      (import "tembra-test:resources/sessions@1.0.0" (instance $sessions
        (export "session" (type (sub resource)))))
      (alias export $sessions "session" (type $session))
      (import "tembra-test:resources/views@1.0.0" (instance
        (alias outer 1 $session (type $s))
        (export "session" (type (eq $s)))
        (export "describe" (func (result string))))))"#;

    fn orb() -> Binding {
        Binding::Orb {
            name: "store".into(),
            api_version: 1,
        }
    }

    /// The dynamic host type of each imported resource, by import and local name.
    fn payloads(contract: &Contract, imports: &[&str]) -> BTreeMap<(String, String), u32> {
        let bindings = imports
            .iter()
            .map(|import| ((*import).to_owned(), orb()))
            .collect();
        layout(contract, &[], &bindings, &Default::default())
            .unwrap()
            .imports
            .into_iter()
            .flat_map(|import| {
                let binding = import.binding;
                import
                    .resources
                    .into_iter()
                    .map(move |(name, payload)| ((binding.clone(), name), payload))
            })
            .collect()
    }

    fn key(import: &str, name: &str) -> (String, String) {
        (import.to_owned(), name.to_owned())
    }

    #[test]
    fn aliases_of_one_resource_link_one_host_type() {
        let contract = Contract::from_dir(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wit/resources"),
            Some("consumer"),
        )
        .unwrap();
        let payloads = payloads(&contract, &[SESSIONS, VIEWS]);
        let session = payloads[&key(SESSIONS, "session")];
        assert_eq!(payloads[&key(VIEWS, "session")], session);
        assert_ne!(payloads[&key(SESSIONS, "counter")], session);
        // Wasmtime type-checks the retained alias against exactly the linked host types.
        let engine = crate::Engine::new(Limits::default()).unwrap();
        let component = Component::new(&engine.engine, wat::parse_str(ALIASING).unwrap()).unwrap();
        let link = |alias: u32| {
            let mut linker = Linker::<()>::new(&engine.engine);
            linker.instance(SESSIONS)?.resource(
                "session",
                ResourceType::host_dynamic(session),
                |_, _| Ok(()),
            )?;
            let mut views = linker.instance(VIEWS)?;
            views.resource("session", ResourceType::host_dynamic(alias), |_, _| Ok(()))?;
            views.func_new("describe", |_, _, _, _| Ok(()))?;
            linker.instantiate_pre(&component).map(drop)
        };
        link(session).unwrap();
        // A type split by local alias, as a per-interface numbering would produce, is refused.
        assert!(link(session + 100).is_err());
    }

    #[test]
    fn distinct_resources_with_one_name_keep_distinct_host_types() {
        let contract = Contract::from_source(
            "same.wit",
            "package test:same@1.0.0;
             interface a { resource session { constructor(); } }
             interface b { resource session { constructor(); } }
             interface c { use a.{session}; ping: func(); }
             world app { import a; import b; import c; }",
            Some("app"),
        )
        .unwrap();
        let (a, b, c) = (
            "test:same/a@1.0.0",
            "test:same/b@1.0.0",
            "test:same/c@1.0.0",
        );
        let payloads = payloads(&contract, &[a, b, c]);
        assert_ne!(payloads[&key(a, "session")], payloads[&key(b, "session")]);
        assert_eq!(payloads[&key(c, "session")], payloads[&key(a, "session")]);
    }
}
