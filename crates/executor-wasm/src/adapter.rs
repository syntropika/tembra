//! Install interpreter-backed callbacks through the existing native Orb facade.
use crate::{
    bindings::{BoundMethod, Environment, Import},
    failure,
    worker::Session,
};
use serde_json::Value;
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tembra::{Orb, Result, Service, core};

/// Install callbacks from the binding plan that `open_port` already checked against WIT.
pub(crate) fn orb(
    metadata: core::Orb,
    session: Session,
    services: Vec<tembra_contracts::ServiceBinding>,
    contract: tembra_contracts::PreparedContract,
    bindings: &BTreeMap<String, tembra_contracts::Binding>,
    hooks: tembra_contracts::HookPlan,
    ingress: Option<tembra_contracts::IngressBinding>,
) -> Result<Orb> {
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
            let mut methods = BTreeMap::new();
            for function in &interface.functions {
                let name = function.name.clone();
                let slot = contract.bind(&binding, &name, false).map_err(failure)?;
                if methods
                    .insert(
                        tembra_contracts::javascript_name(&name),
                        BoundMethod { name, slot },
                    )
                    .is_some()
                {
                    return Err(failure("ambiguous import method"));
                }
            }
            Ok((binding, Rc::new(Import { service, methods })))
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
        for (method, spelling) in service.methods {
            let slot = contract
                .bind(&service.interface, &method, true)
                .map_err(failure)?;
            let contract = contract.clone();
            implementation = implementation.method::<Value, Value, _, _>(
                method,
                move |environment, context, args| {
                    let spelling = spelling.clone();
                    let contract = contract.clone();
                    async move {
                        // Packets spell WIT 64-bit integers as canonical decimal strings.
                        let args = contract.to_wire(slot, true, args).map_err(failure)?;
                        let value = environment.invoke(context, &spelling, args).await?;
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
