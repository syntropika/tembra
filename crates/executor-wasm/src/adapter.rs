//! Install interpreter-backed callbacks through the existing native Orb facade.
use crate::{bindings::Environment, failure, worker::Session};
use serde_json::Value;
use std::{cell::Cell, collections::BTreeMap, rc::Rc};
use tembra::{Orb, Result, Service, core};

/// Install callbacks from the binding plan that `open_port` already checked against WIT.
pub(crate) fn orb(
    metadata: core::Orb,
    session: Session,
    services: Vec<tembra_contracts::ServiceBinding>,
    contract: tembra_contracts::Contract,
    bindings: &BTreeMap<String, tembra_contracts::Binding>,
) -> Result<Orb> {
    let mut orb = Orb::new(&metadata.name, metadata.api_version).isolated();
    let imports = Rc::new(contract.import_services(bindings).map_err(failure)?);
    let contract = Rc::new(contract);
    let next = Rc::new(Cell::new(0_u64));
    let root_session = session.clone();
    let root_imports = imports.clone();
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
        let mut implementation = Service::new(&service.name, move |declaration| {
            let id = allocate_environment(&next)?;
            Environment::create(
                service_session.clone(),
                id,
                declaration,
                Some(create_alias.clone()),
                service_imports.clone(),
                service_contract.clone(),
            )
        })
        .signature(service.signature)
        .lifetime(service.lifetime.into(), service.linger)
        .before_create(move || {
            let session = gate_session.clone();
            async move { session.reserve_factory().await }
        });
        for (method, spelling) in service.methods {
            let called = method.clone();
            let contract = contract.clone();
            let interface = service.interface.clone();
            implementation = implementation.method::<Value, Value, _, _>(
                method,
                move |environment, context, args| {
                    let called = called.clone();
                    let spelling = spelling.clone();
                    let contract = contract.clone();
                    let interface = interface.clone();
                    async move {
                        contract
                            .validate_arguments(&interface, &called, &args)
                            .map_err(failure)?;
                        let value = environment.invoke(context, &spelling, args).await?;
                        contract
                            .validate_result(&interface, &called, &value)
                            .map_err(failure)?;
                        Ok(value)
                    }
                },
            );
        }
        orb = orb.service(implementation);
    }
    Ok(orb)
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
