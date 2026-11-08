//! Inspect resolved contracts or prepare an Orb declaration without evaluating guest code.
use serde::Deserialize;
use std::{collections::BTreeMap, io::Read};
use tembra_contracts::{Binding, Contract};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    name: String,
    api_version: u32,
    bindings: BTreeMap<String, Binding>,
}

/// Emit a machine-readable contract or plan; parser diagnostics go to stderr.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let command = arguments
        .next()
        .ok_or("usage: tembra-contract inspect|plan directory [world]")?;
    let directory = arguments.next().ok_or("missing WIT directory")?;
    let world = arguments.next();
    if arguments.next().is_some() {
        return Err("unexpected argument".into());
    }
    let contract = Contract::from_dir(directory, world.as_deref())?;
    let output = match command.as_str() {
        "inspect" => contract.describe(),
        "plan" => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let registration: Registration = serde_json::from_str(&input)?;
            let plan = contract.plan(
                &registration.name,
                registration.api_version,
                &registration.bindings,
            )?;
            serde_json::json!({"contract":contract.describe(),"orb":{
                "name":plan.orb.name,"apiVersion":plan.orb.api_version,
                "services":plan.orb.services.iter().map(|service| serde_json::json!({"name":service.name,"methods":service.methods,"lifetime":service.lifetime.as_str(),"linger":service.linger_ms})).collect::<Vec<_>>(),
                "requires":plan.orb.requires.iter().map(|requirement| serde_json::json!({"orb":requirement.orb,"apiVersion":requirement.api_version,"services":requirement.methods,"hooks":requirement.hooks})).collect::<Vec<_>>(),"hooks":plan.orb.hooks},"hostImports":plan.host_imports})
        }
        _ => return Err("unknown command; expected inspect or plan".into()),
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}
