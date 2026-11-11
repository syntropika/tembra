use super::*;
use serde_json::{Value, json};
use tembra_contracts::{
    Entry, Field, Function, FunctionKind, Interface, Item, Kind, Model, PreparedContract, Ty,
};

fn contract() -> PreparedContract {
    PreparedContract::new(Model {
        world: "run".into(),
        package: Some("test:browser@1.0.0".into()),
        types: vec![Kind::List(Ty::U8)],
        imports: vec![],
        exports: vec![Entry {
            key: "test:browser/values@1.0.0".into(),
            item: Item::Interface(Interface {
                contract: Some("test:browser/values@1.0.0".into()),
                explicit: true,
                name: Some("values".into()),
                resources: Default::default(),
                functions: vec![Function {
                    name: "read-bytes".into(),
                    kind: FunctionKind::Freestanding,
                    params: vec![Field {
                        name: "count".into(),
                        ty: Ty::U32,
                    }],
                    result: Some(Ty::Ref(0)),
                }],
            }),
        }],
    })
    .unwrap()
}

#[test]
fn browser_artifacts_bind_once_and_validate_by_slot() {
    let bytes = Artifact::package_source("browser", 1, &contract(), "globalThis.x=1").unwrap();
    let artifact = WasmArtifact::new(&bytes).unwrap();
    assert_eq!(artifact.source().unwrap(), "globalThis.x=1");
    let identity: Value = serde_json::from_str(&artifact.description()).unwrap();
    assert_eq!(
        identity,
        json!({"name":"browser","apiVersion":1,"world":"run","kind":"source"})
    );
    let slot = artifact
        .bind("test:browser/values@1.0.0", "read-bytes", true)
        .unwrap();
    assert_eq!(artifact.validate(slot, true, "[3]"), r#"{"ok":null}"#);
    assert_eq!(artifact.validate(slot, false, "[1,2]"), r#"{"ok":null}"#);
    let rejected: Value = serde_json::from_str(&artifact.validate(slot, true, "[-1]")).unwrap();
    assert_eq!(rejected["error"]["reason"], "wit_contract");
    assert!(
        artifact
            .validate(slot + 1, true, "[]")
            .contains("unknown WIT method slot")
    );
    assert!(
        artifact
            .bind("test:browser/values@1.0.0", "readBytes", true)
            .is_err()
    );
    let graph: Value = serde_json::from_str(&artifact.request(r#"{"kind":"describe"}"#)).unwrap();
    assert_eq!(graph["ok"]["javascriptNames"]["read-bytes"], "readBytes");
    let install: Value = serde_json::from_str(
        &artifact.request(r#"{"kind":"install","name":"browser","apiVersion":1}"#),
    )
    .unwrap();
    assert_eq!(
        install["ok"]["plan"]["orb"]["services"][0]["name"],
        "browser::test:browser/values@1.0.0"
    );
    assert_eq!(install["ok"]["bindings"], json!({}));
}

#[test]
fn compiled_and_corrupt_artifacts_fail_explicitly_without_a_parser() {
    let mut compiled = b"\0asm\x0d\0\x01\0".to_vec();
    let source = Artifact::package_source("browser", 1, &contract(), "").unwrap();
    // Keep only the Orb metadata section, which makes this a compiled-component claim.
    let orb_end = source.len() - (2 + "tembra:source".len() + 1);
    compiled.extend_from_slice(&source[8..orb_end]);
    let error = WasmArtifact::new(&compiled).err().unwrap();
    // Built alone (as for the browser) the verifier is absent. Workspace test runs may unify the
    // `authoring` feature, and then the real verifier rejects the empty component instead.
    assert!(
        error.contains("native component verifier") || error.contains("signatures differ"),
        "{error}"
    );
    let mut corrupt = source.clone();
    corrupt[12] ^= 0xff;
    assert!(WasmArtifact::new(&corrupt).is_err());
    assert!(WasmArtifact::new(b"not wasm").is_err());
}

/// A compiled-component claim: only the `tembra:orb` metadata of a source artifact.
fn compiled_claim() -> Vec<u8> {
    let mut compiled = b"\0asm\x0d\0\x01\0".to_vec();
    let source = Artifact::package_source("browser", 1, &contract(), "").unwrap();
    let orb_end = source.len() - (2 + "tembra:source".len() + 1);
    compiled.extend_from_slice(&source[8..orb_end]);
    compiled
}

#[test]
fn prepared_components_bind_metadata_to_registered_digests_only() {
    let compiled = compiled_claim();
    assert!(compiled_artifact(&compiled));
    let source = Artifact::package_source("browser", 1, &contract(), "x").unwrap();
    assert!(!compiled_artifact(&source));
    assert!(!compiled_artifact(b"not wasm"));
    let error = WasmPreparedComponent::new(&compiled, vec![]).err().unwrap();
    assert!(error.contains("no prepared browser adapter"), "{error}");
    // Digests are compared exactly; a near miss or a claim inside the artifact never matches.
    let wrong = "0".repeat(64);
    assert!(WasmPreparedComponent::new(&compiled, vec![wrong.clone()]).is_err());
    let computed =
        WasmPreparedComponent::new(&compiled, vec![wrong, sha256_hex(&compiled)]).unwrap();
    assert_eq!(computed.digest(), sha256_hex(&compiled));
    let identity: Value = serde_json::from_str(&computed.description()).unwrap();
    assert_eq!(
        identity,
        json!({"name":"browser","apiVersion":1,"world":"run","kind":"component"})
    );
    assert_eq!(computed.contributions(), "{}");
    let slot = computed
        .bind("test:browser/values@1.0.0", "read-bytes", true)
        .unwrap();
    assert_eq!(computed.validate(slot, true, "[3]"), r#"{"ok":null}"#);
    // The raw constructor still refuses the same bytes: only the registry path skips the verifier.
    assert!(WasmArtifact::new(&compiled).is_err());
    // Source artifacts never use prepared adapters, even when their digest is registered.
    let error = WasmPreparedComponent::new(&source, vec![sha256_hex(&source)])
        .err()
        .unwrap();
    assert!(error.contains("compiled component"), "{error}");
    // Any byte change breaks the binding.
    let mut changed = compiled.clone();
    *changed.last_mut().unwrap() ^= 1;
    assert!(WasmPreparedComponent::new(&changed, vec![sha256_hex(&compiled)]).is_err());
}

/// Lowercase hexadecimal SHA-256, independent of the code under test.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
