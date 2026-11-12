//! Compiled components whose every function traps, built from a WIT world alone.
//!
//! wit-component's dummy module imports and exports exactly what the world names, with the real
//! canonical ABI, so a test can check what linking, instantiation and declarations do before any
//! guest code runs, for worlds a hand-written core module could not easily express (an incoming
//! HTTP handler, an `async func` hook).
#![allow(dead_code)]

/// Package a trapping component of `world` in `wit` as an Orb artifact with the given roles.
/// The standard packages (WASI, the kernel's `tembra:guest`) resolve as for any contract.
pub fn artifact(
    wit: &str,
    world: &str,
    name: &str,
    roles: &tembra_contracts::ComponentRoles,
) -> Vec<u8> {
    let contract = tembra_contracts::Contract::from_source("orb.wit", wit, Some(world)).unwrap();
    let encoded = contract.encode().unwrap();
    let wit_component::DecodedWasm::WitPackage(resolve, package) =
        wit_component::decode(&encoded).unwrap()
    else {
        panic!("the contract encodes a WIT package");
    };
    let selected = resolve.select_world(&[package], Some(world)).unwrap();
    let mut core =
        wit_component::dummy_module(&resolve, selected, wit_parser::ManglingAndAbi::Standard32);
    wit_component::embed_component_metadata(
        &mut core,
        &resolve,
        selected,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    let component = wit_component::ComponentEncoder::default()
        .module(&core)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap();
    contract
        .component_artifact(&component, name, 1, roles)
        .unwrap()
}
