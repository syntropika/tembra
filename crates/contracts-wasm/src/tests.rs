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

const VIEW: &str = "test:keyed/counter-ref@1.0.0";
const SERVICE: &str = "test:keyed/counter@1.0.0";

/// A router importing the keyed view `counter-ref` of the provider service `counter`.
fn view_contract() -> PreparedContract {
    use tembra_contracts::ResourceIdentity;
    let router = Entry {
        key: "test:keyed/router@1.0.0".into(),
        item: Item::Interface(Interface {
            contract: Some("test:keyed/router@1.0.0".into()),
            explicit: true,
            name: Some("router".into()),
            resources: Default::default(),
            functions: vec![Function {
                name: "route".into(),
                kind: FunctionKind::Freestanding,
                params: vec![],
                result: Some(Ty::String),
            }],
        }),
    };
    PreparedContract::new(Model {
        world: "compiled-router".into(),
        package: Some("test:keyed@1.0.0".into()),
        types: vec![
            Kind::Resource(ResourceIdentity {
                interface: Some(VIEW.into()),
                name: Some("counter".into()),
            }),
            Kind::Borrow(0),
            Kind::Own(0),
            Kind::List(Ty::String),
            Kind::Result {
                ok: Some(Ty::Ref(2)),
                err: Some(Ty::String),
            },
        ],
        imports: vec![Entry {
            key: VIEW.into(),
            item: Item::Interface(Interface {
                contract: Some(VIEW.into()),
                explicit: true,
                name: Some("counter-ref".into()),
                resources: [("counter".into(), 0)].into(),
                functions: vec![
                    Function {
                        name: "[method]counter.add".into(),
                        kind: FunctionKind::Method(0),
                        params: vec![
                            Field {
                                name: "self".into(),
                                ty: Ty::Ref(1),
                            },
                            Field {
                                name: "amount".into(),
                                ty: Ty::U32,
                            },
                        ],
                        result: Some(Ty::U32),
                    },
                    Function {
                        name: "get".into(),
                        kind: FunctionKind::AsyncFreestanding,
                        params: vec![Field {
                            name: "key".into(),
                            ty: Ty::Ref(3),
                        }],
                        result: Some(Ty::Ref(4)),
                    },
                ],
            }),
        }],
        exports: vec![router],
    })
    .unwrap()
}

/// The provider exporting the service the view addresses.
fn provider_contract() -> PreparedContract {
    PreparedContract::new(Model {
        world: "provider".into(),
        package: Some("test:keyed@1.0.0".into()),
        types: vec![],
        imports: vec![],
        exports: vec![Entry {
            key: SERVICE.into(),
            item: Item::Interface(Interface {
                contract: Some(SERVICE.into()),
                explicit: true,
                name: Some("counter".into()),
                resources: Default::default(),
                functions: vec![Function {
                    name: "add".into(),
                    kind: FunctionKind::Freestanding,
                    params: vec![Field {
                        name: "amount".into(),
                        ty: Ty::U32,
                    }],
                    result: Some(Ty::U32),
                }],
            }),
        }],
    })
    .unwrap()
}

/// Unsigned LEB128, the length encoding of custom sections and Orb metadata.
fn leb(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A compiled-component claim whose Orb metadata carries one reference role.
///
/// Packaging normally checks roles natively; this spells the metadata directly so the browser
/// decoder alone is exercised. A source payload ends with its empty reference map (`0`).
fn reference_claim(contract: &PreparedContract, binding: &str, target: &str) -> Vec<u8> {
    let source = Artifact::package_source("router", 1, contract, "").unwrap();
    let mut reader = &source[8..];
    // Read the leading `tembra:orb` custom section: id, size, name, payload.
    assert_eq!(reader[0], 0);
    reader = &reader[1..];
    let (mut size, mut shift, mut used) = (0usize, 0, 0);
    while {
        size |= usize::from(reader[used] & 0x7f) << shift;
        shift += 7;
        used += 1;
        reader[used - 1] & 0x80 != 0
    } {}
    let section = &reader[used..used + size];
    let name = 1 + "tembra:orb".len();
    let mut payload = section[name..].to_vec();
    assert_eq!(
        payload.pop(),
        Some(0),
        "source payloads end with no reference roles"
    );
    payload.push(1);
    for value in [binding, target] {
        leb(value.len(), &mut payload);
        payload.extend_from_slice(value.as_bytes());
    }
    let mut body = section[..name].to_vec();
    body.extend_from_slice(&payload);
    let mut compiled = b"\0asm\x0d\0\x01\0".to_vec();
    compiled.push(0);
    leb(body.len(), &mut compiled);
    compiled.extend_from_slice(&body);
    compiled
}

#[test]
fn prepared_components_expose_checked_reference_roles_without_a_parser() {
    let claim = reference_claim(&view_contract(), VIEW, SERVICE);
    let component = WasmPreparedComponent::new(&claim, vec![sha256_hex(&claim)]).unwrap();
    let provider = Artifact::package_source("keyed", 1, &provider_contract(), "").unwrap();
    let provider = WasmArtifact::new(&provider).unwrap();
    let provided: Value = serde_json::from_str(
        &provider.request(r#"{"kind":"install","name":"keyed","apiVersion":1}"#),
    )
    .unwrap();
    let signature = provided["ok"]["plan"]["orb"]["services"][0]["signature"].clone();
    assert!(signature.is_string(), "{provided}");

    let described: Value =
        serde_json::from_str(&component.request(r#"{"kind":"describe"}"#)).unwrap();
    assert_eq!(
        described["ok"]["references"],
        json!({VIEW: {
            "interface": SERVICE,
            "resource": {"interface": VIEW, "name": "counter"},
            "acquire": "get",
            "methods": {"[method]counter.add": "add"},
            "signature": signature,
        }})
    );
    let install: Value = serde_json::from_str(&component.request(&format!(
        r#"{{"kind":"install","name":"router","apiVersion":1,"requires":{{"{VIEW}":{{"orb":"keyed","apiVersion":1}}}}}}"#
    )))
    .unwrap();
    let qualified = format!("keyed::{SERVICE}");
    assert_eq!(
        install["ok"]["importServices"][VIEW], qualified,
        "{install}"
    );
    assert_eq!(
        install["ok"]["plan"]["orb"]["requires"][0]["signatures"][&qualified],
        signature
    );
    assert_eq!(
        install["ok"]["plan"]["orb"]["requires"][0]["services"][&qualified],
        json!(["add"])
    );
    // View method slots carry only data; the implicit receiver never crosses as JSON.
    let add = component.bind(VIEW, "[method]counter.add", false).unwrap();
    assert_eq!(component.validate(add, true, "[5]"), r#"{"ok":null}"#);
    assert!(component.validate(add, true, "[0,5]").contains("error"));
    assert_eq!(component.validate(add, false, "7"), r#"{"ok":null}"#);

    // Without the role, the same metadata is an ordinary resource import the data profile refuses.
    let plain = compiled_claim_of(&view_contract());
    let plain = WasmPreparedComponent::new(&plain, vec![sha256_hex(&plain)]).unwrap();
    let refused: Value = serde_json::from_str(&plain.request(&format!(
        r#"{{"kind":"install","name":"router","apiVersion":1,"requires":{{"{VIEW}":{{"orb":"keyed","apiVersion":1}}}}}}"#
    )))
    .unwrap();
    assert!(refused.get("error").is_some(), "{refused}");

    // Malformed roles are refused by the browser decoder as well.
    for (binding, target) in [(VIEW, VIEW), ("test:keyed/missing@1.0.0", SERVICE)] {
        let claim = reference_claim(&view_contract(), binding, target);
        assert!(WasmPreparedComponent::new(&claim, vec![sha256_hex(&claim)]).is_err());
    }
    // Source artifacts cannot carry reference roles.
    let claim = reference_claim(&view_contract(), VIEW, SERVICE);
    let mut source = claim.clone();
    source.extend_from_slice(&[0, 14, 13]);
    source.extend_from_slice(b"tembra:source");
    assert!(WasmArtifact::new(&source).is_err());
}

/// A compiled-component claim with the metadata of `contract` and no roles.
fn compiled_claim_of(contract: &PreparedContract) -> Vec<u8> {
    let mut compiled = b"\0asm\x0d\0\x01\0".to_vec();
    let source = Artifact::package_source("router", 1, contract, "").unwrap();
    let orb_end = source.len() - (2 + "tembra:source".len() + 1);
    compiled.extend_from_slice(&source[8..orb_end]);
    compiled
}
