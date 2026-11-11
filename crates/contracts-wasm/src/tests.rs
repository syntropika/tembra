use super::*;
use serde_json::{Value, json};
use tembra_contracts::{Contract, SharedRuntime};

const COUNTER: &str = "package example:counter@1.0.0;
interface counter { add: func(amount: u64) -> u64; whoami: func() -> string; }
interface clock { now: func() -> u64; }
world counter-orb { export counter; }
world compiled { import clock; export counter; }
world other { export clock; }";

fn contract(world: &str) -> Contract {
    Contract::from_source("counter.wit", COUNTER, Some(world)).unwrap()
}

/// A real component whose core module implements `world` (a compiled-language stand-in).
fn compiled_component(world: &str) -> Vec<u8> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("counter.wit", COUNTER).unwrap();
    let world = resolve.select_world(&[package], Some(world)).unwrap();
    let mut module =
        wit_component::dummy_module(&resolve, world, wit_parser::ManglingAndAbi::Standard32);
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
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

/// Append a custom section, as a forger would.
fn with_section(mut bytes: Vec<u8>, name: &str, data: &[u8]) -> Vec<u8> {
    let mut payload = vec![u8::try_from(name.len()).unwrap()];
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(data);
    bytes.push(0);
    let mut size = payload.len();
    loop {
        let byte = u8::try_from(size & 0x7f).unwrap();
        size >>= 7;
        if size == 0 {
            bytes.push(byte);
            break;
        }
        bytes.push(byte | 0x80);
    }
    bytes.extend_from_slice(&payload);
    bytes
}

#[test]
fn compiled_components_are_verified_and_report_exact_imports() {
    let component = compiled_component("compiled");
    let bytes = contract("compiled")
        .component_artifact(&component, "counter", 1, &Default::default())
        .unwrap();
    let artifact = WasmArtifact::new(&bytes).unwrap();
    assert!(!artifact.source_runtime());
    let identity: Value = serde_json::from_str(&artifact.description()).unwrap();
    assert_eq!(
        identity,
        json!({"name":"counter","apiVersion":1,"world":"compiled","kind":"component"})
    );
    let imports: Value = serde_json::from_str(&artifact.imports()).unwrap();
    assert_eq!(imports, json!(["example:counter/clock@1.0.0"]));
    assert_eq!(artifact.component(), bytes);
    assert_eq!(artifact.contributions(), "{}");
    let slot = artifact
        .bind("example:counter/counter@1.0.0", "add", true)
        .unwrap();
    assert_eq!(
        artifact.validate(slot, true, r#"["18446744073709551615"]"#),
        r#"{"ok":null}"#
    );
    let rejected: Value =
        serde_json::from_str(&artifact.validate(slot, true, r#"["18446744073709551616"]"#))
            .unwrap();
    assert_eq!(rejected["error"]["reason"], "wit_contract");
}

#[test]
fn metadata_never_substitutes_for_the_actual_component() {
    // The component implements a different world than its packaged contract claims.
    let mismatched = with_section(
        compiled_component("counter-orb"),
        "tembra:orb",
        &[], // replaced below by a real section from another contract
    );
    assert!(WasmArtifact::new(&mismatched).is_err());
    let other = contract("compiled")
        .component_artifact(
            &compiled_component("compiled"),
            "counter",
            1,
            &Default::default(),
        )
        .unwrap();
    // A forged runtime marker on a component without the private lifecycle is refused.
    let forged = with_section(other.clone(), "tembra:runtime", b"tembra:orb@0.1.0");
    let error = WasmArtifact::new(&forged).err().unwrap();
    assert!(error.contains("lifecycle"), "{error}");
    // The retired source-text section is unknown metadata: such an artifact is refused.
    let legacy = with_section(other.clone(), "tembra:source", b"export const counter = {}");
    let error = WasmArtifact::new(&legacy).err().unwrap();
    assert!(error.contains("unsupported artifact metadata"), "{error}");
    let mut corrupt = other;
    corrupt[12] ^= 0xff;
    assert!(WasmArtifact::new(&corrupt).is_err());
    assert!(WasmArtifact::new(b"not wasm").is_err());
}

/// The executor's real source component: verified lifecycle, versioned runtime internals.
#[test]
#[ignore = "requires TEMBRA_SOURCE_COUNTER"]
fn source_components_are_verified_from_their_actual_types() {
    let component = std::fs::read(std::env::var_os("TEMBRA_SOURCE_COUNTER").unwrap()).unwrap();
    // A componentizer's output, packed as production builds pack it.
    let component = SharedRuntime::pinned().pack(&component).unwrap();
    let bytes = contract("counter-orb")
        .source_component_artifact(&component, "counter", 1)
        .unwrap();
    // Browser qualification harnesses package the same verified bytes they execute.
    if let Some(out) = std::env::var_os("TEMBRA_PACKAGE_OUT") {
        std::fs::write(out, &bytes).unwrap();
    }
    let artifact = WasmArtifact::new(&bytes).unwrap();
    assert!(artifact.source_runtime());
    let imports: Vec<String> = serde_json::from_str(&artifact.imports()).unwrap();
    assert!(imports.contains(&"tembra:orb/host@0.1.0".to_owned()));
    assert!(
        imports
            .iter()
            .filter(|name| name.starts_with("wasi:"))
            .all(|name| tembra_contracts::runtime_internal(name)),
        "{imports:?}"
    );
    // The same bytes repackaged for a contract they do not implement are refused.
    let wrong = contract("other").source_component_artifact(&component, "counter", 1);
    assert!(wrong.is_err());
}

#[test]
fn releasing_component_storage_preserves_checked_call_signatures() {
    let bytes = contract("compiled")
        .component_artifact(
            &compiled_component("compiled"),
            "counter",
            1,
            &Default::default(),
        )
        .unwrap();
    let mut artifact = WasmArtifact::new(&bytes).unwrap();
    let identity = artifact.description();
    let imports = artifact.imports();
    let slot = artifact
        .bind("example:counter/counter@1.0.0", "whoami", true)
        .unwrap();
    let arguments = artifact.validate(slot, true, "[]");
    let result = artifact.validate(slot, false, "\"alice\"");
    artifact.release_component();
    assert!(artifact.component().is_empty());
    assert_eq!(artifact.description(), identity);
    assert_eq!(artifact.imports(), imports);
    assert_eq!(
        artifact
            .bind("example:counter/counter@1.0.0", "whoami", true)
            .unwrap(),
        slot
    );
    assert_eq!(artifact.validate(slot, true, "[]"), arguments);
    assert_eq!(artifact.validate(slot, false, "\"alice\""), result);
}

/// A real component for `world` with the pinned runtime defined, uninstantiated, at its root.
fn with_runtime(world: &Contract) -> Vec<u8> {
    let encoded = world.encode().unwrap();
    let wit_parser::decoding::DecodedWasm::WitPackage(resolve, package) =
        wit_parser::decoding::decode(&encoded).unwrap()
    else {
        panic!("a WIT package");
    };
    let selected = resolve
        .select_world(&[package], Some(world.prepared().world()))
        .unwrap();
    let mut module =
        wit_component::dummy_module(&resolve, selected, wit_parser::ManglingAndAbi::Standard32);
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        selected,
        wit_component::StringEncoding::UTF8,
        false,
    )
    .unwrap();
    let mut bytes = wit_component::ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap();
    // A module section holding the runtime core, appended at the root.
    let runtime = SharedRuntime::pinned().bytes();
    bytes.push(1);
    let mut size = runtime.len();
    loop {
        let byte = u8::try_from(size & 0x7f).unwrap();
        size >>= 7;
        if size == 0 {
            bytes.push(byte);
            break;
        }
        bytes.push(byte | 0x80);
    }
    bytes.extend_from_slice(runtime);
    bytes
}

#[test]
fn source_components_importing_the_shared_runtime_expand_only_with_the_pinned_core() {
    let public = contract("counter-orb");
    let world = public.source_world(&Default::default()).unwrap();
    let fat = with_runtime(&world);
    let thin = SharedRuntime::pinned().pack(&fat).unwrap();
    let bytes = public
        .source_component_artifact(&thin, "counter", 1)
        .unwrap();
    let mut artifact = WasmArtifact::new(&bytes).unwrap();
    assert!(artifact.source_runtime() && artifact.requires_runtime());
    // Verified and retained thin; the runtime import is the host's, not a WIT import.
    assert_eq!(artifact.component(), bytes);
    let imports: Vec<String> = serde_json::from_str(&artifact.imports()).unwrap();
    assert!(
        imports
            .iter()
            .all(|name| !name.starts_with("tembra-runtime-")),
        "{imports:?}"
    );
    assert_eq!(WasmSharedRuntime::digest(), SharedRuntime::SHA256);
    let runtime = WasmSharedRuntime::new(SharedRuntime::pinned().bytes()).unwrap();
    let expanded = artifact.expanded_component(&runtime).unwrap();
    assert_eq!(expanded, [&fat[..], &bytes[thin.len()..]].concat());
    // Any other runtime bytes are refused before an expansion exists.
    let mut other = SharedRuntime::pinned().bytes().to_vec();
    other[100] ^= 1;
    assert!(WasmSharedRuntime::new(&other).is_err());
    artifact.release_component();
    assert!(artifact.expanded_component(&runtime).is_err());
    // Compiled components are self-contained and never expand.
    let compiled = contract("compiled")
        .component_artifact(
            &compiled_component("compiled"),
            "counter",
            1,
            &Default::default(),
        )
        .unwrap();
    let compiled = WasmArtifact::new(&compiled).unwrap();
    assert!(!compiled.requires_runtime());
    let error = compiled.expanded_component(&runtime).err().unwrap();
    assert!(
        error.contains("does not import the shared runtime"),
        "{error}"
    );
}
