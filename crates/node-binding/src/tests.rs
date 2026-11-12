//! Packaging a compiled component through `NodeContract::component_artifact`, the path the SDK's
//! `build()` takes for a `.wasm` entry, with the roles a component built with `#[orb]` carries.
use super::NodeContract;
use napi::bindgen_prelude::Uint8Array;
use std::collections::HashMap;
use tembra_contracts::{Artifact, Contract, HookComposition, Lifetime, ServicePolicy};

/// A world declaring or contributing to a hook, `guard`, next to a service, `ledger`.
const WIT: &str = "package test:book@1.0.0; interface guard { apply: func(value: u32) -> u32; } interface ledger { count: func() -> u32; } world book { import guard; export guard; export ledger; }";
const GUARD: &str = "test:book/guard@1.0.0";
const LEDGER: &str = "test:book/ledger@1.0.0";

/// A component of the world whose core module carries `manifest` in its `tembra:manifest`
/// section, as the component linker leaves the section the macro writes.
fn component(manifest: &str) -> Vec<u8> {
    let manifest = manifest.replace('"', "\\\"");
    wat::parse_str(format!(
        r#"(component
  (import "{GUARD}" (instance (export "apply" (func (param "value" u32) (result u32)))))
  (core module $orb
    (func (export "apply") (param i32) (result i32) local.get 0)
    (func (export "count") (result i32) i32.const 0)
    (@custom "tembra:manifest" "{manifest}"))
  (core instance $orb (instantiate $orb))
  (func $apply (param "value" u32) (result u32) (canon lift (core func $orb "apply")))
  (func $count (result u32) (canon lift (core func $orb "count")))
  (instance $guard (export "apply" (func $apply)))
  (instance $ledger (export "count" (func $count)))
  (export "{GUARD}" (instance $guard))
  (export "{LEDGER}" (instance $ledger)))"#
    ))
    .unwrap()
}

/// Package the component carrying `manifest` as the SDK does, with the given contributions.
fn package(manifest: &str, contributes: &[(&str, f64)]) -> napi::Result<Artifact> {
    let contract = NodeContract {
        contract: Contract::from_source("book.wit", WIT, Some("book")).unwrap(),
    };
    let contributes: HashMap<String, f64> = contributes
        .iter()
        .map(|(hook, priority)| ((*hook).to_owned(), *priority))
        .collect();
    let bytes = contract.component_artifact(
        Uint8Array::new(component(manifest)),
        "book".into(),
        1,
        (!contributes.is_empty()).then_some(contributes),
        None,
    )?;
    Ok(Artifact::decode(&bytes).unwrap())
}

/// A hook the component declares and its service's lifetime survive packaging without any role
/// passed by the caller.
#[test]
fn a_declared_hook_and_a_service_policy_survive_node_packaging() {
    let artifact = package(
        &format!(
            r#"{{"declares":{{"{GUARD}":"waterfall"}},"services":{{"{LEDGER}":{{"lifetime":"host","linger_ms":0}}}}}}"#
        ),
        &[],
    )
    .unwrap();
    assert_eq!(
        artifact.hooks,
        [(GUARD.to_owned(), HookComposition::Waterfall)].into()
    );
    assert_eq!(
        artifact.services,
        [(
            LEDGER.to_owned(),
            ServicePolicy {
                lifetime: Lifetime::Host,
                linger_ms: 0,
            },
        )]
        .into()
    );
    assert!(artifact.contributions.is_empty());
}

/// Contributions come from the component and from the caller; a hook both name keeps the
/// priority they agree on, and two different priorities are refused with both named.
#[test]
fn contributions_come_from_the_component_and_the_caller_alike() {
    let carried = format!(r#"{{"contributes":{{"{GUARD}":5}}}}"#);
    for contributes in [&[][..], &[(GUARD, 5.0)]] {
        let artifact = package(&carried, contributes).unwrap();
        assert_eq!(artifact.contributions, [(GUARD.to_owned(), 5)].into());
    }
    let artifact = package("{}", &[(GUARD, -2.0)]).unwrap();
    assert_eq!(artifact.contributions, [(GUARD.to_owned(), -2)].into());
    let refusal = package(&carried, &[(GUARD, 2.0)]).err().unwrap();
    assert_eq!(
        refusal.reason,
        format!("{GUARD}: the component contributes at priority 5, `contributes` at priority 2")
    );
    // A hook the component declares takes no contribution from the caller.
    let declared = format!(r#"{{"declares":{{"{GUARD}":"waterfall"}}}}"#);
    assert!(package(&declared, &[(GUARD, 1.0)]).is_err());
}
