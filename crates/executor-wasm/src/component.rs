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
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::mpsc,
};
use tembra::Result;
use tembra_contracts::{
    FunctionKind, HookComposition, Item, Plan, PreparedContract, ServiceBinding, ServicePolicy,
    guest, javascript_name,
};
pub(crate) use worker::State as ComponentState;

/// Describe the plain interface exports a compiled artifact implements, with its roles.
///
/// A compiled service's lifetime and linger come from the artifact's service policies, when one
/// names the service; it has no access policy. Hook callbacks and the kernel's own callbacks
/// (`tembra:guest`) are answered to their callers, never offered as services; the hooks the
/// artifact declares are described under their export's alias, as a source bundle declares its
/// own.
pub(crate) fn describe(
    contract: &PreparedContract,
    name: &str,
    api_version: u32,
    contributions: &BTreeMap<String, i32>,
    hooks: &BTreeMap<String, HookComposition>,
    policies: &BTreeMap<String, ServicePolicy>,
) -> Value {
    let mut services = Map::new();
    let mut declared = Map::new();
    for entry in &contract.model().exports {
        let Item::Interface(interface) = &entry.item else {
            continue;
        };
        // The incoming HTTP handler is declared like a service so `check_ingress` consumes it;
        // it never becomes a portable service binding.
        if interface.contract.is_none() || interface.functions.is_empty() {
            continue;
        }
        let alias = javascript_name(interface.name.as_deref().unwrap_or_default());
        if let Some(composition) = interface
            .contract
            .as_ref()
            .and_then(|identity| hooks.get(identity))
        {
            declared.insert(alias, json!(composition));
            continue;
        }
        if interface.contract.as_ref().is_some_and(|identity| {
            contributions.contains_key(identity) || guest::reserved(identity)
        }) {
            continue;
        }
        let methods = interface
            .functions
            .iter()
            .map(|function| javascript_name(&function.name))
            .collect::<Vec<_>>();
        let mut service = Map::from_iter([("methods".to_owned(), json!(methods))]);
        if let Some(policy) = interface
            .contract
            .as_ref()
            .and_then(|identity| policies.get(identity))
        {
            service.insert("lifetime".into(), json!(policy.lifetime));
            service.insert("linger".into(), json!(policy.linger_ms));
        }
        services.insert(alias, Value::Object(service));
    }
    json!({ "name": name, "apiVersion": api_version, "services": services, "hooks": declared, "contributes": contributions })
}

/// Resolve checked exports and selected Orb imports into Send-only Store dispatch tables.
fn layout(
    contract: &PreparedContract,
    services: &[ServiceBinding],
    bindings: &BTreeMap<String, Binding>,
    hooks: &tembra_contracts::HookPlan,
) -> Result<worker::Layout> {
    let mut exports = Vec::new();
    // Every callable export is keyed by its function id: the export slot `bind` returns, the
    // same id the interpreter's Load table and every host's commands use.
    for service in services {
        let mut functions = BTreeMap::new();
        for method in service.methods.keys() {
            let slot = contract
                .bind(&service.interface, method, true)
                .map_err(failure)?;
            let role = resource_roles(contract, slot)?;
            let roles = (role.receiver.is_some() || role.own_result.is_some()).then_some(role);
            functions.insert(
                slot,
                worker::ExportFunction {
                    name: method.clone(),
                    roles,
                },
            );
        }
        exports.push(worker::Export {
            interface: service.interface.clone(),
            service: service.alias.clone(),
            functions,
        });
    }
    let mut contributions = BTreeSet::new();
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
        exports.push(worker::Export {
            interface: contribution.name.clone(),
            service: contribution.alias.clone(),
            functions: [(
                contribution.slot,
                worker::ExportFunction {
                    name: function.name.clone(),
                    roles: None,
                },
            )]
            .into(),
        });
        contributions.insert(contribution.slot);
    }
    let mut imports = Vec::new();
    // One dynamic host type per defining resource: every alias of it, in any selected import,
    // links the same type, and distinct resources (even with equal names) never share one.
    let mut payloads = BTreeMap::<(String, String), u32>::new();
    for entry in &contract.model().imports {
        let contribution = hooks
            .contributions
            .iter()
            .any(|contribution| contribution.binding == entry.key);
        let role = match bindings.get(&entry.key) {
            Some(Binding::Orb { .. }) if contribution => worker::Role::Contribution,
            Some(Binding::Orb { .. }) => worker::Role::Dependency,
            Some(Binding::Kernel) => worker::Role::Kernel,
            Some(Binding::Hook) => worker::Role::Hook,
            Some(Binding::Host { .. }) | None => continue,
        };
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
        let view = contract.references().get(&entry.key);
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
                let acquire = view.filter(|view| view.acquire == function.name);
                if let Some(view) = acquire {
                    return Ok(worker::ImportFunction {
                        name: function.name.clone(),
                        spelling: javascript_name(&function.name),
                        asynchronous: true,
                        receiver: None,
                        own_result: None,
                        stream: false,
                        acquire: payload(Some(&view.resource))?,
                    });
                }
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
                    acquire: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        imports.push(worker::Import {
            binding: entry.key.clone(),
            resources,
            functions,
            role,
            reference: view.is_some(),
        });
    }
    // Each declared hook's import binding, to the alias `hook.declare` names.
    let declared = hooks
        .hooks
        .iter()
        .filter_map(|hook| {
            contract
                .model()
                .imports
                .iter()
                .find_map(|entry| match &entry.item {
                    Item::Interface(interface)
                        if interface.contract.as_deref() == Some(&hook.name) =>
                    {
                        Some((entry.key.clone(), hook.alias.clone()))
                    }
                    _ => None,
                })
        })
        .collect();
    Ok(worker::Layout {
        exports,
        imports,
        contributions,
        ingress: contract.ingress().map(str::to_owned),
        state_init: guest::exports(contract.model(), guest::STATE_INIT),
        resource_init: guest::exports(contract.model(), guest::RESOURCE_INIT),
        hooks: declared,
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
mod state_tests {
    use super::*;
    use tembra_contracts::{Contract, Implementation};

    const WIT: &str = "package example:counter@1.0.0;
        interface counter { add: func(by: s64) -> s64; }
        world stateful {
            import tembra:guest/state@1.0.0;
            export tembra:guest/state-init@1.0.0;
            import tembra:guest/resources@1.0.0;
            export tembra:guest/resources-init@1.0.0;
            export counter;
        }";

    /// A service's lifetime and linger come from its policy and reach the checked binding; a
    /// service no policy names keeps the defaults.
    #[test]
    fn service_policies_describe_each_service_lifetime_and_linger() {
        let contract = Contract::from_source("counter.wit", WIT, None).unwrap();
        let policies = BTreeMap::from([(
            "example:counter/counter@1.0.0".to_owned(),
            tembra_contracts::ServicePolicy {
                lifetime: tembra_contracts::Lifetime::Host,
                linger_ms: 0,
            },
        )]);
        let description = describe(
            &contract,
            "counter",
            1,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &policies,
        );
        assert_eq!(
            description["services"]["counter"],
            json!({ "methods": ["add"], "lifetime": "host", "linger": 0 })
        );
        let implementation: Implementation = serde_json::from_value(description).unwrap();
        let services = contract
            .check_implementation("counter", 1, implementation)
            .unwrap();
        assert_eq!(services[0].lifetime, tembra_contracts::Lifetime::Host);
        let description = describe(
            &contract,
            "counter",
            1,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(
            description["services"]["counter"],
            json!({ "methods": ["add"] })
        );
    }

    /// The kernel interfaces are neither services nor dependencies: the layout links the state
    /// import to the kernel and remembers the state-init export the worker calls on creation.
    #[test]
    fn kernel_state_is_linked_to_the_kernel_and_never_described_as_a_service() {
        let contract = Contract::from_source("counter.wit", WIT, None).unwrap();
        let bindings = contract
            .providers(&Default::default(), &Grants::default())
            .unwrap();
        let description = describe(
            &contract,
            "counter",
            1,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(
            description["services"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["counter"]
        );
        let implementation: Implementation = serde_json::from_value(description).unwrap();
        let services = contract
            .check_implementation("counter", 1, implementation.clone())
            .unwrap();
        let hooks = contract.check_hooks(&implementation, &bindings).unwrap();
        let layout = layout(&contract, &services, &bindings, &hooks).unwrap();
        assert!(layout.state_init);
        assert!(layout.resource_init);
        assert_eq!(
            layout
                .exports
                .iter()
                .map(|export| export.service.as_str())
                .collect::<Vec<_>>(),
            ["counter"]
        );
        assert_eq!(layout.imports.len(), 2);
        assert_eq!(layout.imports[0].binding, guest::STATE);
        assert_eq!(layout.imports[0].role, worker::Role::Kernel);
        assert_eq!(
            layout.imports[0]
                .functions
                .iter()
                .map(|function| function.name.as_str())
                .collect::<Vec<_>>(),
            ["read", "update"]
        );
        assert_eq!(layout.imports[1].binding, guest::RESOURCE);
        assert_eq!(layout.imports[1].role, worker::Role::Kernel);
    }
}

#[cfg(test)]
mod hook_tests {
    use super::*;
    use tembra_contracts::{Contract, Implementation};

    const WIT: &str = "package example:gate@1.0.0;
        interface guard { record admission { value: s32, blocked: bool } apply: func(input: admission) -> admission; }
        interface admissions { use guard.{admission}; admit: func(value: s32) -> admission; }
        world gate { import guard; export guard; export admissions; }";

    /// A declared hook is described under its export's alias, never as a service; its import
    /// binds to the plan and the layout remembers the alias the worker declares it by.
    #[test]
    fn a_declared_hook_is_described_and_its_import_runs_the_plan() {
        let contract = Contract::from_source("gate.wit", WIT, None).unwrap();
        let identity = "example:gate/guard@1.0.0";
        let bindings = contract
            .providers(&Default::default(), &Grants::default())
            .unwrap();
        assert_eq!(bindings[identity], Binding::Hook);
        let declared = BTreeMap::from([(
            identity.to_owned(),
            tembra_contracts::HookComposition::Waterfall,
        )]);
        let description = describe(
            &contract,
            "gate",
            1,
            &BTreeMap::new(),
            &declared,
            &BTreeMap::new(),
        );
        assert_eq!(
            description["services"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["admissions"]
        );
        assert_eq!(description["hooks"], json!({ "guard": "waterfall" }));
        let implementation: Implementation = serde_json::from_value(description).unwrap();
        let services = contract
            .check_implementation("gate", 1, implementation.clone())
            .unwrap();
        let hooks = contract.check_hooks(&implementation, &bindings).unwrap();
        let layout = layout(&contract, &services, &bindings, &hooks).unwrap();
        assert_eq!(
            layout
                .exports
                .iter()
                .map(|export| export.service.as_str())
                .collect::<Vec<_>>(),
            ["admissions"]
        );
        assert_eq!(layout.imports.len(), 1);
        assert_eq!(layout.imports[0].binding, identity);
        assert_eq!(layout.imports[0].role, worker::Role::Hook);
        assert_eq!(
            layout.hooks,
            BTreeMap::from([(identity.to_owned(), "guard".to_owned())])
        );
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
