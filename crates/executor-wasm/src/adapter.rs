//! Install interpreter-backed callbacks through the existing native Orb facade.
use crate::{
    bindings::{BoundMethod, Environment, Import, Roles},
    failure,
    worker::Session,
};
use serde_json::Value;
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tembra::{Orb, Result, Service, core};

/// Install callbacks from the binding plan that `open_port` already checked against WIT.
///
/// `capabilities` is set only for a revision admitted to the native capability transport: its
/// functions with resource or byte-stream positions travel as capabilities. Every other
/// revision, and every data-only function, keeps data-only callbacks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn orb(
    metadata: core::Orb,
    session: Session,
    services: Vec<tembra_contracts::ServiceBinding>,
    contract: tembra_contracts::PreparedContract,
    bindings: &BTreeMap<String, tembra_contracts::Binding>,
    hooks: tembra_contracts::HookPlan,
    ingress: Option<tembra_contracts::IngressBinding>,
    capabilities: bool,
) -> Result<Orb> {
    let roles = |slot| {
        if capabilities {
            Roles::of(&contract, slot)
        } else {
            Ok(Roles::default())
        }
    };
    let mut orb = Orb::new(&metadata.name, metadata.api_version).isolated();
    let imports = contract
        .import_services(bindings)
        .map_err(failure)?
        .into_iter()
        .filter(|(binding, _)| {
            !hooks
                .contributions
                .iter()
                .any(|contribution| &contribution.binding == binding)
        })
        .map(|(binding, service)| {
            let entry = contract
                .model()
                .imports
                .iter()
                .find(|entry| entry.key == binding)
                .ok_or_else(|| failure("missing prepared import"))?;
            let tembra_contracts::Item::Interface(interface) = &entry.item else {
                return Err(failure("Orb import must be an interface"));
            };
            let view = contract.references().get(&binding);
            let mut methods = BTreeMap::new();
            for function in &interface.functions {
                if view.is_some_and(|view| function.name == view.acquire) {
                    continue;
                }
                let name = function.name.clone();
                let slot = contract.bind(&binding, &name, false).map_err(failure)?;
                if methods
                    .insert(
                        tembra_contracts::javascript_name(&name),
                        BoundMethod {
                            name: view
                                .and_then(|view| view.methods.get(&name))
                                .cloned()
                                .unwrap_or(name),
                            slot,
                            roles: if view.is_some() {
                                Roles::of(&contract, slot)?
                            } else {
                                roles(slot)?
                            },
                        },
                    )
                    .is_some()
                {
                    return Err(failure("ambiguous import method"));
                }
            }
            let requirement = requirement(&metadata.requires, &service);
            Ok((
                binding.clone(),
                Rc::new(Import {
                    service,
                    methods,
                    requirement,
                    reference: view
                        .map(|view| crate::bindings::nominal(&view.resource))
                        .transpose()?,
                }),
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let imports = Rc::new(imports);
    for binding in &hooks.hooks {
        use tembra_contracts::HookComposition;
        orb = match binding.composition {
            HookComposition::Series => orb.hook(
                tembra::Hook::<Value, ()>::series(&binding.name).signature(&binding.signature),
            ),
            HookComposition::Waterfall => orb.hook(
                tembra::Hook::<Value, Value>::waterfall(&binding.name)
                    .signature(&binding.signature),
            ),
            HookComposition::Bail => orb.hook(
                tembra::Hook::<Value, Option<Value>>::bail(&binding.name)
                    .signature(&binding.signature),
            ),
        };
    }
    for binding in hooks.contributions {
        orb = orb.contribute_portable(
            binding.target.clone(),
            binding.name.clone(),
            binding.signature.clone(),
            binding.priority,
            move |context, value| {
                let binding = binding.clone();
                async move {
                    let environment = context.orb::<Environment>()?;
                    environment.contribute(context, &binding, value).await
                }
            },
        );
    }
    let hook_bindings = Rc::new(
        hooks
            .hooks
            .into_iter()
            .map(|binding| (binding.alias.clone(), binding))
            .collect::<BTreeMap<_, _>>(),
    );
    let root_hooks = hook_bindings.clone();
    let next = Rc::new(Cell::new(0_u64));
    let root_session = session.clone();
    let root_imports = imports.clone();
    // The prepared contract is shared internally, so each clone only adds a reference.
    let root_contract = contract.clone();
    let root_next = next.clone();
    orb = orb.create(move |declaration| {
        Environment::create(
            root_session.clone(),
            allocate_environment(&root_next)?,
            declaration,
            None,
            root_imports.clone(),
            root_contract.clone(),
            root_hooks.clone(),
        )
    });
    for requirement in metadata.requires {
        orb = orb.requires(requirement);
    }
    for service in services {
        let service_session = session.clone();
        let next = next.clone();
        let create_alias = service.alias;
        let service_imports = imports.clone();
        let service_contract = contract.clone();
        let gate_session = session.clone();
        let service_hooks = hook_bindings.clone();
        let mut implementation = Service::new(&service.name, move |declaration| {
            let id = allocate_environment(&next)?;
            Environment::create(
                service_session.clone(),
                id,
                declaration,
                Some(create_alias.clone()),
                service_imports.clone(),
                service_contract.clone(),
                service_hooks.clone(),
            )
        })
        .signature(service.signature)
        .lifetime(service.lifetime.into(), service.linger)
        .before_create(move || {
            let session = gate_session.clone();
            async move { session.reserve_factory().await }
        });
        // The guest selects each method by its export slot through the Load table.
        for method in service.methods.into_keys() {
            let slot = contract
                .bind(&service.interface, &method, true)
                .map_err(failure)?;
            let roles = Rc::new(roles(slot)?);
            if !roles.is_data() {
                // Implicit `self`, owned results and stream ends travel as capabilities, never
                // as data. A stream's call keeps working after its head.
                implementation =
                    implementation.payload_method(method, move |environment, context, payload| {
                        let roles = roles.clone();
                        async move {
                            if roles.stream {
                                return environment.invoke_stream(context, slot, payload).await;
                            }
                            environment
                                .invoke_resource(context, slot, &roles, payload)
                                .await
                                .map(tembra::portable::Outcome::from)
                        }
                    });
                continue;
            }
            let contract = contract.clone();
            implementation = implementation.method::<Value, Value, _, _>(
                method,
                move |environment, context, args| {
                    let contract = contract.clone();
                    async move {
                        // Packets spell WIT 64-bit integers as canonical decimal strings.
                        let args = contract.to_wire(slot, true, args).map_err(failure)?;
                        let value = environment.invoke(context, slot, args).await?;
                        contract.from_wire(slot, false, value).map_err(failure)
                    }
                },
            );
        }
        orb = orb.service(implementation);
    }
    if let Some(binding) = ingress {
        orb = orb.service(handler(
            binding,
            session,
            imports,
            contract,
            hook_bindings,
            next,
        ));
    }
    Ok(orb)
}

/// The exported-function table delivered once with an interpreter Load.
///
/// Every function of an exported interface with an identity is listed under the slot that
/// `bind` returns for it, the same id every host sends in `invoke` and `contribute` commands.
/// The incoming HTTP handler is served only through `ingress`, so it has no id.
/// Which ids are services, contributions or neither is decided per command by the guest from
/// its own Orb and the environment's service; the table only names functions.
pub(crate) fn exported_functions(
    contract: &tembra_contracts::PreparedContract,
) -> Result<Vec<crate::protocol::ExportedInterface>> {
    let mut exports = Vec::new();
    for entry in &contract.model().exports {
        let tembra_contracts::Item::Interface(interface) = &entry.item else {
            continue;
        };
        let Some(identity) = &interface.contract else {
            continue;
        };
        if interface.functions.is_empty() || identity == tembra_contracts::HTTP_HANDLER {
            continue;
        }
        let functions = interface
            .functions
            .iter()
            .map(|function| {
                let slot = contract
                    .bind(identity, &function.name, true)
                    .map_err(failure)?;
                Ok((slot, tembra_contracts::javascript_name(&function.name)))
            })
            .collect::<Result<Vec<_>>>()?;
        exports.push(crate::protocol::ExportedInterface {
            interface: identity.clone(),
            service: tembra_contracts::javascript_name(
                interface.name.as_deref().unwrap_or_default(),
            ),
            functions,
        });
    }
    Ok(exports)
}

/// The host-local service through which the facade calls the incoming HTTP handler.
///
/// It has no signature and no portable entrypoint: WIT dependency admission cannot select it,
/// and every portable call is refused by the kernel. Its typed argument is constructed only by
/// `crate::ingress::serve`.
fn handler(
    binding: tembra_contracts::IngressBinding,
    session: Session,
    imports: Rc<BTreeMap<String, Rc<Import>>>,
    contract: tembra_contracts::PreparedContract,
    hooks: Rc<BTreeMap<String, tembra_contracts::HookBinding>>,
    next: Rc<Cell<u64>>,
) -> Service<Environment> {
    let gate = session.clone();
    let alias = binding.alias;
    let method = Rc::<str>::from(binding.method);
    Service::new(binding.name, move |declaration| {
        Environment::create(
            session.clone(),
            allocate_environment(&next)?,
            declaration,
            Some(alias.clone()),
            imports.clone(),
            contract.clone(),
            hooks.clone(),
        )
    })
    .lifetime(binding.lifetime.into(), binding.linger)
    .before_create(move || {
        let session = gate.clone();
        async move { session.reserve_factory().await }
    })
    .local_method(
        crate::ingress::HANDLE,
        move |environment: Rc<Environment>, context, ingress: crate::ingress::Ingress| {
            let method = method.clone();
            async move { environment.ingress(context, &method, ingress).await }
        },
    )
}

/// The installed requirement applied to runtime acquisition of an imported service.
///
/// It is the rule a declared reference uses (`Declaration::get`): the guest names only the import
/// binding, never a version or method set. The planner records a requirement for every Orb
/// binding, including one naming the importing Orb itself, so a planned import always has one.
fn requirement(
    requires: &[tembra::core::Requirement],
    service: &str,
) -> Option<tembra::AcquisitionOptions> {
    requires
        .iter()
        .find(|requirement| requirement.methods.contains_key(service))
        .map(|requirement| tembra::AcquisitionOptions {
            api_version: requirement.api_version,
            methods: requirement.methods[service].clone(),
        })
}

/// Root and object redeclarations share an identity sequence and never overwrite an old heap slot.
fn allocate_environment(next: &Cell<u64>) -> Result<u64> {
    let id = next
        .get()
        .checked_add(1)
        .filter(|id| *id <= core::MAX_ID)
        .ok_or_else(|| failure("environment space exhausted"))?;
    next.set(id);
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Even an import bound to the importing Orb itself is planned as a requirement carrying the
    /// binding's API version and the whole interface, so runtime acquisition never runs without
    /// the installed requirement and the SDK's own-service resolution names the same methods.
    #[test]
    fn every_orb_binding_plans_the_requirement_runtime_acquisition_applies() {
        let wit = "package example:keyed@1.0.0;
interface counter { add: func(amount: u32) -> u32; who: func() -> string; }
interface router { route: func(key: string) -> u32; }
world consumer { import counter; export router; }";
        let contract =
            tembra_contracts::Contract::from_source("contract.wit", wit, Some("consumer")).unwrap();
        for provider in ["router", "keyed"] {
            let bindings = BTreeMap::from([(
                "example:keyed/counter@1.0.0".to_owned(),
                tembra_contracts::Binding::Orb {
                    name: provider.into(),
                    api_version: 3,
                },
            )]);
            let plan = contract.plan("router", 1, &bindings).unwrap();
            let service = format!("{provider}::example:keyed/counter@1.0.0");
            let resolved = requirement(&plan.orb.requires, &service).expect(provider);
            assert_eq!(resolved.api_version, 3);
            assert_eq!(
                resolved.methods,
                ["add", "who"].map(str::to_owned).into_iter().collect()
            );
        }
    }
}
