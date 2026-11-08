//! Install interpreter-backed callbacks through the existing native Orb facade.
use crate::{bindings::Environment, failure, worker::Session};
use serde::Deserialize;
use serde_json::Value;
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};
use tembra::{Orb, Result, Service, core};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Description {
    name: Option<String>,
    api_version: Option<u32>,
    services: BTreeMap<String, ServiceDescription>,
}

#[derive(Deserialize)]
struct ServiceDescription {
    methods: BTreeSet<String>,
    lifetime: Option<String>,
    linger: Option<u32>,
    access: bool,
}

/// Match executable callbacks to resolved WIT exports before publishing kernel metadata.
pub(crate) fn orb(
    metadata: core::Orb,
    session: Session,
    description: Value,
    contract: tembra_contracts::Contract,
    bindings: &BTreeMap<String, tembra_contracts::Binding>,
) -> Result<Orb> {
    let mut description: Description = serde_json::from_value(description).map_err(failure)?;
    if description
        .name
        .as_ref()
        .is_some_and(|name| name != &metadata.name)
        || description
            .api_version
            .is_some_and(|version| version != metadata.api_version)
    {
        return Err(failure("bundle identity differs from its registration"));
    }
    let mut orb = Orb::new(&metadata.name, metadata.api_version);
    let imports = Rc::new(imports(&contract, bindings)?);
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
    for service in metadata.services {
        let contract_id = service
            .name
            .strip_prefix(&format!("{}::", metadata.name))
            .ok_or_else(|| failure("invalid prepared service name"))?;
        let alias = tembra_contracts::javascript_name(
            contract_id
                .rsplit('/')
                .next()
                .and_then(|name| name.split('@').next())
                .ok_or_else(|| failure("invalid interface identity"))?,
        );
        let implementation = description
            .services
            .remove(&alias)
            .ok_or_else(|| failure(format!("missing implementation for {contract_id}")))?;
        if implementation.methods
            != service
                .methods
                .iter()
                .map(|method| tembra_contracts::javascript_name(method))
                .collect()
        {
            return Err(failure(format!(
                "implementation methods differ from WIT for {contract_id}"
            )));
        }
        if implementation.access {
            return Err(failure(
                "service access policies require a prepared view adapter",
            ));
        }
        let lifetime = match implementation.lifetime.as_deref().unwrap_or("referenced") {
            "referenced" => core::Lifetime::Referenced,
            "host" => core::Lifetime::Host,
            _ => return Err(failure("invalid service lifetime")),
        };
        let linger = implementation.linger.unwrap_or(0);
        let service_session = session.clone();
        let next = next.clone();
        let create_alias = alias.clone();
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
        .lifetime(lifetime, linger)
        .before_create(move || {
            let session = gate_session.clone();
            async move { session.reserve_factory().await }
        });
        for method in service.methods {
            let called = method.clone();
            let contract = contract.clone();
            let interface = service
                .name
                .strip_prefix(&format!("{}::", metadata.name))
                .expect("prepared service")
                .to_owned();
            implementation = implementation.method::<Value, Value, _, _>(
                method,
                move |environment, context, args| {
                    let called = called.clone();
                    let contract = contract.clone();
                    let interface = interface.clone();
                    async move {
                        contract
                            .validate_arguments(&interface, &called, &args)
                            .map_err(failure)?;
                        let value = environment
                            .invoke(context, &tembra_contracts::javascript_name(&called), args)
                            .await?;
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
    if !description.services.is_empty() {
        return Err(failure("bundle exports services absent from its WIT world"));
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

/// Preserve each explicit import alias instead of silently choosing the last provider.
fn imports(
    contract: &tembra_contracts::Contract,
    bindings: &BTreeMap<String, tembra_contracts::Binding>,
) -> Result<BTreeMap<String, String>> {
    let description = contract.describe();
    let mut result = BTreeMap::new();
    for item in description["imports"]
        .as_array()
        .expect("resolved world imports")
    {
        let Some(binding) = item["binding"].as_str() else {
            continue;
        };
        let Some(tembra_contracts::Binding::Orb { name, .. }) = bindings.get(binding) else {
            continue;
        };
        let contract = item["contract"]
            .as_str()
            .ok_or_else(|| failure("anonymous import needs an explicit mapping"))?;
        result.insert(
            binding.to_owned(),
            tembra_contracts::service_name(name, contract),
        );
    }
    Ok(result)
}
