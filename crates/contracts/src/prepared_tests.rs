//! Runtime-only checks: prepared contracts are decoded as untrusted input without a WIT parser.
use super::*;
use crate::artifact::{COMPONENT_HEADER, append_section, encode_orb};
use serde_json::json;

fn function(name: &str, params: Vec<Field>, result: Option<Ty>) -> Function {
    Function {
        name: name.into(),
        kind: FunctionKind::Freestanding,
        params,
        result,
    }
}

fn field(name: &str, ty: Ty) -> Field {
    Field {
        name: name.into(),
        ty,
    }
}

/// A small world built directly from the typed model.
fn model() -> Model {
    Model {
        world: "run".into(),
        package: Some("test:prepared@1.0.0".into()),
        types: vec![
            Kind::Record(vec![field("item-name", Ty::String)]),
            Kind::List(Ty::U8),
            Kind::Option(Ty::Ref(0)),
        ],
        imports: vec![Entry {
            key: "test:prepared/store@1.0.0".into(),
            item: Item::Interface(Interface {
                contract: Some("test:prepared/store@1.0.0".into()),
                explicit: true,
                name: Some("store".into()),
                resources: false,
                functions: vec![function(
                    "fetch",
                    vec![field("key", Ty::String)],
                    Some(Ty::Ref(2)),
                )],
            }),
        }],
        exports: vec![Entry {
            key: "test:prepared/values@1.0.0".into(),
            item: Item::Interface(Interface {
                contract: Some("test:prepared/values@1.0.0".into()),
                explicit: true,
                name: Some("values".into()),
                resources: false,
                functions: vec![function(
                    "read-bytes",
                    vec![field("item", Ty::Ref(0))],
                    Some(Ty::Ref(1)),
                )],
            }),
        }],
    }
}

fn rejected(model: Model, expected: &str) {
    let error = PreparedContract::new(model.clone()).unwrap_err();
    assert!(error.0.contains(expected), "{error}");
    // The same model is rejected identically after a wire round trip.
    let mut writer = codec::Writer::default();
    codec::write_model(&mut writer, &model);
    assert!(PreparedContract::decode(&writer.finish()).is_err());
}

#[test]
fn prepared_contracts_round_trip_and_bind_slots() {
    let contract = PreparedContract::new(model()).unwrap();
    let bytes = contract.encode();
    let decoded = PreparedContract::decode(&bytes).unwrap();
    assert_eq!(decoded, contract);
    assert_eq!(decoded.encode(), bytes);
    let export = decoded
        .bind("test:prepared/values@1.0.0", "read-bytes", true)
        .unwrap();
    let import = decoded
        .bind("test:prepared/store@1.0.0", "fetch", false)
        .unwrap();
    assert_ne!(export, import);
    decoded
        .validate_slot(export, true, &json!([{"itemName": "x"}]))
        .unwrap();
    decoded
        .validate_slot(export, false, &json!([1, 2]))
        .unwrap();
    assert!(decoded.validate_slot(export, false, &json!([256])).is_err());
    assert!(decoded.validate_slot(import, false, &json!(null)).is_ok());
    assert!(decoded.validate_slot(99, true, &json!([])).is_err());
    // Exports bind by interface identity, imports by world binding; sides never mix.
    assert!(
        decoded
            .bind("test:prepared/values@1.0.0", "read-bytes", false)
            .is_err()
    );
    assert!(
        decoded
            .bind("test:prepared/store@1.0.0", "fetch", true)
            .is_err()
    );
    assert!(
        decoded
            .bind("test:prepared/values@1.0.0", "readBytes", true)
            .is_err()
    );
    assert_eq!(
        decoded.validate_packet(export, false, "[1]"),
        r#"{"ok":null}"#
    );
    assert!(
        decoded
            .validate_packet(export, false, "[")
            .contains("\"error\"")
    );
    let described = decoded.describe();
    assert_eq!(described["javascriptNames"]["read-bytes"], "readBytes");
    assert_eq!(described["javascriptNames"]["item-name"], "itemName");
    assert_eq!(described["types"][2]["definition"]["kind"]["option"], 0);
}

#[test]
fn corrupt_references_cycles_and_handles_are_rejected() {
    let mut forward = model();
    forward.types[0] = Kind::Alias(Ty::Ref(1));
    rejected(forward, "out of order");
    let mut cycle = model();
    cycle.types[0] = Kind::Alias(Ty::Ref(0));
    rejected(cycle, "out of order");
    let mut range = model();
    if let Item::Interface(interface) = &mut range.exports[0].item {
        interface.functions[0].result = Some(Ty::Ref(40));
    }
    rejected(range, "out of range");
    let mut handle = model();
    handle.types.push(Kind::Own(1));
    rejected(handle, "does not name a resource");
    let mut method = model();
    if let Item::Interface(interface) = &mut method.exports[0].item {
        interface.functions[0].kind = FunctionKind::Method(0);
    }
    rejected(method, "does not name a resource");
    let mut deep = model();
    for index in 0..80 {
        deep.types.push(Kind::List(Ty::Ref(2 + index)));
    }
    rejected(deep, "nesting");
}

#[test]
fn corrupt_names_duplicates_and_limits_are_rejected() {
    let mut duplicate = model();
    duplicate.types[0] = Kind::Record(vec![field("a", Ty::U8), field("a", Ty::U8)]);
    rejected(duplicate, "duplicate prepared field");
    let mut functions = model();
    if let Item::Interface(interface) = &mut functions.exports[0].item {
        let copy = interface.functions[0].clone();
        interface.functions.push(copy);
    }
    rejected(functions, "duplicate prepared function");
    let mut items = model();
    let copy = items.exports[0].clone();
    items.exports.push(copy);
    rejected(items, "duplicate prepared world item");
    let mut name = model();
    name.types[0] = Kind::Record(vec![field("bad name", Ty::U8)]);
    rejected(name, "invalid prepared WIT name");
    let mut identity = model();
    identity.exports[0].key = "test:prepared/\"values\"".into();
    rejected(identity, "invalid prepared WIT identity");
    let mut wide = model();
    wide.types[0] = Kind::Tuple(vec![Ty::U8; 2000]);
    rejected(wide, "too many members");
    let mut many = model();
    many.types = vec![Kind::Enum(vec!["a".into()]); 5000];
    rejected(many, "too many types");
}

/// A world whose `levels` definitions each reference the previous one twice, doubling the
/// expanded signature per level while the encoded contract grows by a few bytes.
fn doubling(levels: u32, functions: usize, level: impl Fn(Ty) -> Kind) -> Model {
    let mut types = vec![Kind::Record(vec![field("leaf", Ty::U32)])];
    types.extend((0..levels).map(|previous| level(Ty::Ref(previous))));
    Model {
        world: "run".into(),
        package: Some("test:prepared@1.0.0".into()),
        types,
        imports: vec![],
        exports: vec![Entry {
            key: "test:prepared/values@1.0.0".into(),
            item: Item::Interface(Interface {
                contract: Some("test:prepared/values@1.0.0".into()),
                explicit: true,
                name: Some("values".into()),
                resources: false,
                functions: (0..functions)
                    .map(|index| {
                        function(
                            &format!("read{index}"),
                            vec![field("value", Ty::Ref(levels))],
                            None,
                        )
                    })
                    .collect(),
            }),
        }],
    }
}

fn record(previous: Ty) -> Kind {
    Kind::Record(vec![field("left", previous), field("right", previous)])
}

#[test]
fn shared_type_graphs_cannot_amplify_signature_expansion() {
    let started = std::time::Instant::now();
    // Forty levels stay within the nesting limit but would expand to roughly 2^40 nodes.
    for level in [record as fn(Ty) -> Kind, |previous| {
        Kind::Tuple(vec![previous; 2])
    }] {
        let model = doubling(40, 1, level);
        rejected(model.clone(), "expand beyond their size limit");
        let mut contract = codec::Writer::default();
        codec::write_model(&mut contract, &model);
        let contract = contract.finish();
        assert!(contract.len() < 1024, "{} bytes", contract.len());
        let mut orb = codec::Writer::default();
        orb.str("prepared");
        orb.u32(1);
        orb.bytes(&contract);
        let mut bytes = COMPONENT_HEADER.to_vec();
        append_section(&mut bytes, "tembra:orb", &orb.finish());
        append_section(&mut bytes, "tembra:source", b"globalThis.__tembraOrb={}");
        let error = Artifact::decode(&bytes).err().unwrap();
        assert!(error.0.contains("expand beyond"), "{error}");
    }
    // The world total is bounded too: each signature fits alone, three together do not.
    rejected(doubling(19, 3, record), "expand beyond their size limit");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    // Moderate sharing remains an ordinary contract usable by every runtime query.
    let shared = PreparedContract::new(doubling(12, 4, record)).unwrap();
    shared.check_value_profile().unwrap();
    let plan = shared.plan("prepared", 1, &Default::default()).unwrap();
    assert!(plan.orb.services[0].signature.is_some());
    let slot = shared
        .bind("test:prepared/values@1.0.0", "read0", true)
        .unwrap();
    assert!(shared.validate_slot(slot, true, &json!([1])).is_err());
}

#[test]
fn malformed_encodings_are_rejected_before_allocation() {
    let bytes = PreparedContract::new(model()).unwrap().encode();
    for end in 0..bytes.len() {
        assert!(
            PreparedContract::decode(&bytes[..end]).is_err(),
            "prefix {end}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(PreparedContract::decode(&trailing).is_err());
    let mut version = bytes.clone();
    version[0] = 9;
    assert!(PreparedContract::decode(&version).is_err());
    // Version, then a world name whose length is a padded (non-canonical) LEB128.
    assert!(PreparedContract::decode(&[1, 0x83, 0x00, b'r', b'u', b'n']).is_err());
    // A count far larger than the remaining bytes is refused before allocating.
    let mut huge = vec![1, 3, b'r', b'u', b'n', 0];
    huge.extend([0xff, 0xff, 0xff, 0xff, 0x0f]);
    assert!(PreparedContract::decode(&huge).is_err());
    // An unknown type tag.
    let mut tag = vec![1, 3, b'r', b'u', b'n', 0, 1, 99];
    tag.extend([0, 0]);
    assert!(PreparedContract::decode(&tag).is_err());
    assert!(PreparedContract::decode(&vec![0; artifact_limit()]).is_err());
}

fn artifact_limit() -> usize {
    (4 << 20) + 1
}

fn source_artifact(contract: &PreparedContract, source: &str) -> Vec<u8> {
    let identity = Identity {
        name: "prepared".into(),
        api_version: 1,
        world: contract.world().into(),
    };
    let mut bytes = COMPONENT_HEADER.to_vec();
    append_section(&mut bytes, "tembra:orb", &encode_orb(&identity, contract));
    append_section(&mut bytes, "tembra:source", source.as_bytes());
    bytes
}

#[test]
fn runtime_artifacts_decode_source_without_a_parser_and_refuse_extra_content() {
    let contract = PreparedContract::new(model()).unwrap();
    let bytes = source_artifact(&contract, "globalThis.__tembraOrb={}");
    let artifact = Artifact::decode(&bytes).unwrap();
    assert_eq!(artifact.contract, contract);
    assert_eq!(artifact.identity.world, "run");
    assert!(matches!(
        artifact.implementation,
        ArtifactImplementation::Source(ref source) if source == "globalThis.__tembraOrb={}"
    ));
    let mut foreign = bytes.clone();
    append_section(&mut foreign, "producers", b"x");
    assert!(Artifact::decode(&foreign).is_err());
    let mut code = bytes.clone();
    code.extend([1, 1, 0]);
    assert!(Artifact::decode(&code).is_err());
    let mut module = bytes.clone();
    module[4..8].copy_from_slice(&[1, 0, 0, 0]);
    assert!(Artifact::decode(&module).is_err());
    let mut truncated = bytes.clone();
    truncated.pop();
    assert!(Artifact::decode(&truncated).is_err());
    let mut duplicate = bytes.clone();
    append_section(&mut duplicate, "tembra:source", b"other");
    assert!(Artifact::decode(&duplicate).is_err());
    // Source Orbs must satisfy the interpreter value profile on load.
    let mut context = model();
    context.types[1] = Kind::List(Ty::ErrorContext);
    let context = PreparedContract::new(context).unwrap();
    assert!(Artifact::decode(&source_artifact(&context, "")).is_err());
}

#[cfg(not(feature = "authoring"))]
#[test]
fn runtime_without_the_component_verifier_refuses_compiled_orbs() {
    let contract = PreparedContract::new(model()).unwrap();
    let identity = Identity {
        name: "prepared".into(),
        api_version: 1,
        world: "run".into(),
    };
    let mut bytes = COMPONENT_HEADER.to_vec();
    append_section(&mut bytes, "tembra:orb", &encode_orb(&identity, &contract));
    let error = Artifact::decode(&bytes).err().unwrap();
    assert!(error.0.contains("native component verifier"), "{error}");
}
