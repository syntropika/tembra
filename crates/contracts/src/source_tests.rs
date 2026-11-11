use super::*;
use std::collections::{BTreeMap, BTreeSet};

const HOOKS: &str = r#"
package example:hooks@1.0.0;
interface guard { apply: func(input: u64) -> u64; }
interface runner { run: func(input: u64) -> u64; }
interface other { ping: func(); }
world provider { export guard; export runner; }
world stray { export guard; export runner; import other; }
"#;

/// Encode a real component whose core module implements every function of `contract`'s world.
fn component(contract: &Contract) -> Vec<u8> {
    let mut module = wit_component::dummy_module(
        &contract.resolve,
        contract.world,
        wit_parser::ManglingAndAbi::Standard32,
    );
    wit_component::embed_component_metadata(
        &mut module,
        &contract.resolve,
        contract.world,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    wit_component::ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

fn provider() -> Contract {
    Contract::from_source("hooks.wit", HOOKS, Some("provider")).unwrap()
}

fn hooks(aliases: &[&str]) -> BTreeSet<String> {
    aliases.iter().map(|alias| (*alias).to_owned()).collect()
}

#[test]
fn the_source_world_adds_only_the_private_package_and_named_hook_imports() {
    let world = provider().source_world(&hooks(&["guard"])).unwrap();
    let keys = |entries: &[crate::Entry]| {
        entries
            .iter()
            .map(|entry| entry.key.clone())
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        keys(&world.model().exports),
        BTreeSet::from([
            "example:hooks/guard@1.0.0".into(),
            "example:hooks/runner@1.0.0".into(),
            LIFECYCLE.into(),
        ])
    );
    assert_eq!(
        keys(&world.model().imports),
        BTreeSet::from([HOST.into(), "example:hooks/guard@1.0.0".into()])
    );
    // Only an exported interface can become a hook self-import.
    assert!(provider().source_world(&hooks(&["missing"])).is_err());
}

#[test]
fn prepared_source_components_package_the_public_contract_and_are_verified_on_decode() {
    let public = provider();
    let built = component(&public.source_world(&hooks(&["guard"])).unwrap());
    let bytes = public
        .source_component_artifact(&built, "guarded", 1)
        .unwrap();
    let artifact = Artifact::decode(&bytes).unwrap();
    assert!(artifact.source_runtime);
    assert!(artifact.contributions.is_empty());
    assert_eq!(artifact.contract.world(), "provider");
    assert_eq!(artifact.component, bytes);
}

#[test]
fn foreign_components_are_held_to_the_strict_public_contract() {
    let public = provider();
    let plain = component(&public);
    // Without the lifecycle export, the runtime section cannot make a component a source Orb.
    assert!(
        public
            .source_component_artifact(&plain, "plain", 1)
            .is_err()
    );
    let mut forged = public
        .component_artifact(&plain, "plain", 1, &BTreeMap::new())
        .unwrap();
    crate::artifact::append_section(&mut forged, "tembra:runtime", SOURCE_PACKAGE.as_bytes());
    let Err(error) = Artifact::decode(&forged) else {
        panic!("forged runtime section accepted")
    };
    assert!(error.0.contains(LIFECYCLE), "{error}");
    // A source-built component is not a valid compiled Orb: its extra exports are refused.
    let built = component(&public.source_world(&BTreeSet::new()).unwrap());
    assert!(
        public
            .component_artifact(&built, "built", 1, &BTreeMap::new())
            .is_err()
    );
}

#[test]
fn imports_outside_the_contract_and_private_package_are_refused() {
    let stray = Contract::from_source("hooks.wit", HOOKS, Some("stray")).unwrap();
    let built = component(&stray.source_world(&BTreeSet::new()).unwrap());
    let Err(error) = provider().source_component_artifact(&built, "stray", 1) else {
        panic!("undeclared import accepted")
    };
    assert!(error.0.contains("import"), "{error}");
}

#[test]
fn other_runtime_packages_are_refused() {
    let public = provider();
    let built = component(&public.source_world(&BTreeSet::new()).unwrap());
    let mut bytes = public.source_component_artifact(&built, "old", 1).unwrap();
    let position = bytes
        .windows(SOURCE_PACKAGE.len())
        .rposition(|window| window == SOURCE_PACKAGE.as_bytes())
        .unwrap();
    bytes[position + SOURCE_PACKAGE.len() - 1] = b'9';
    assert!(Artifact::decode(&bytes).is_err());
}

#[test]
fn the_tembra_namespace_is_reserved() {
    let contract = Contract::from_source(
        "mine.wit",
        "package tembra:mine@1.0.0; interface api { f: func(); } world w { export api; }",
        None,
    )
    .unwrap();
    assert!(contract.source_world(&BTreeSet::new()).is_err());
}

#[test]
fn only_wasi_0_2_runtime_internals_are_recognized() {
    assert!(runtime_internal("wasi:clocks/wall-clock@0.2.12"));
    assert!(runtime_internal("wasi:cli/environment@0.2.0"));
    assert!(!runtime_internal("wasi:clocks/wall-clock@0.3.0"));
    assert!(!runtime_internal("wasi:sockets/tcp@0.2.0"));
    assert!(!runtime_internal("wasi:http/types@0.2.0"));
    assert!(!runtime_internal("wasi:clocks/wall-clock@0.2.0-rc"));
}
