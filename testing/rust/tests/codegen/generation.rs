use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::json;
use tembra_codegen::generate_rust;
use tembra_codegen::{GenerationOptions, NativeMethodBinding, generate_rust_with_options};
use tembra_contracts::{Contract, Member, Placement, ValueSchema};

fn portable(schema: serde_json::Value) -> Option<ValueSchema> {
    Some(ValueSchema::Portable(schema))
}

fn fixture() -> Contract {
    let presence = json!({
        "type": "object",
        "properties": {
            "arr": {"type": "array", "items": {"type": "integer"}},
            "nonempty": {"type": "array", "items": {"type": "integer"}, "minItems": 1},
            "nullable": {"anyOf": [{"type": "null"}, {"type": "string"}]},
            "float": {"anyOf": [{"type": "number"}, {"type": "null"}]},
            "map": {"type": "object", "properties": {}, "additionalProperties": true},
            "nested": {
                "type": "object",
                "properties": {
                    "items": {"type": "array", "items": {"type": "string"}},
                    "maybe": {"anyOf": [{"type": "null"}, {"type": "integer"}]}
                },
                "additionalProperties": false
            }
        },
        "additionalProperties": false
    });
    let open_object = json!({
        "type": "object",
        "properties": {"type": {"type": "string"}},
        "required": ["type"],
        "additionalProperties": true
    });
    Contract::local(
        "example.generated",
        1,
        json!({
            "type": "object",
            "properties": {"name": {"type": "string", "default": "default"}}
        }),
        BTreeMap::from([
            (
                "add".into(),
                Member::Method {
                    args: portable(json!({
                        "type": "object",
                        "properties": {"by": {"type": "integer"}},
                        "required": ["by"],
                        "additionalProperties": false
                    })),
                    result: portable(json!({"type": "integer"})),
                },
            ),
            (
                "nullable".into(),
                Member::Method {
                    args: portable(json!({"type": "null"})),
                    result: portable(json!({"type": "null"})),
                },
            ),
            (
                "finite".into(),
                Member::Method {
                    args: portable(json!({"anyOf": [{"type": "number"}, {"type": "null"}]})),
                    result: portable(json!({"type": "boolean"})),
                },
            ),
            (
                "type".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            ),
            (
                "reference".into(),
                Member::Method {
                    args: None,
                    result: portable(json!({"type": "integer"})),
                },
            ),
            (
                "check".into(),
                Member::Method {
                    args: portable(json!({"type": "string", "enum": ["ready", "done"]})),
                    result: portable(json!({"type": "boolean"})),
                },
            ),
            (
                "echo".into(),
                Member::Method {
                    args: portable(open_object.clone()),
                    result: portable(open_object),
                },
            ),
            (
                "defaults".into(),
                Member::Method {
                    args: portable(json!({
                        "type": "object",
                        "properties": {"value": {"type": "string", "default": "must-not-insert"}}
                    })),
                    result: portable(json!({"type": "boolean"})),
                },
            ),
            (
                "presence".into(),
                Member::Method {
                    args: portable(presence.clone()),
                    result: portable(presence),
                },
            ),
        ]),
    )
    .unwrap()
}

#[test]
fn output_is_deterministic_and_retains_metadata() {
    let contract = fixture();
    let generated = generate_rust(&contract).unwrap();
    assert_eq!(generated, generate_rust(&contract).unwrap());
    assert!(generated.contains("DESCRIPTOR_JSON"));
    assert!(generated.contains("call_json("));
    assert!(!generated.contains("pub async fn call_type("));
    assert!(generated.contains("pub async fn r#type("));
    assert!(generated.contains("impl ::tembra::ContractToken for Token"));
    assert!(generated.contains("pub fn with_context("));
    let remote = Contract::remote(
        "example.remote",
        1,
        json!({"type": "object", "properties": {}}),
        Placement::Named {
            name: "service".into(),
        },
        BTreeMap::from([
            (
                "main".into(),
                BTreeMap::from([(
                    "count".into(),
                    Member::State {
                        schema: json!({"type": "integer"}),
                    },
                )]),
            ),
            ("readonly".into(), BTreeMap::new()),
        ]),
    )
    .unwrap();
    let generated = generate_rust(&remote).unwrap();
    assert!(generated.contains("variant_main"));
    assert!(generated.contains("variant_readonly"));
    assert!(generated.contains("StateCount"));
    assert!(generated.contains("STATE_MEMBERS"));
}

#[test]
fn rejects_unmapped_native_signatures_and_name_collisions() {
    let native = Contract::local(
        "native",
        1,
        json!({"type": "object", "properties": {}}),
        BTreeMap::from([(
            "run".into(),
            Member::Method {
                args: Some(ValueSchema::Native("value@1".into())),
                result: None,
            },
        )]),
    )
    .unwrap();
    assert_eq!(
        generate_rust(&native).unwrap_err().reason(),
        "unmapped_native"
    );
    let opaque = Contract::local(
        "opaque",
        1,
        json!({"type": "object", "properties": {}}),
        BTreeMap::from([(
            "native".into(),
            Member::Opaque {
                id: "capability@1".into(),
            },
        )]),
    )
    .unwrap();
    assert_eq!(
        generate_rust(&opaque).unwrap_err().reason(),
        "unmapped_native"
    );
    let colliding = Contract::local(
        "collision",
        1,
        json!({"type": "object", "properties": {}}),
        BTreeMap::from([
            (
                "foo-bar".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            ),
            (
                "foo_bar".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            ),
        ]),
    )
    .unwrap();
    assert_eq!(
        generate_rust(&colliding).unwrap_err().reason(),
        "name_collision"
    );
    let colliding = Contract::local(
        "field.collision",
        1,
        json!({
            "type": "object",
            "properties": {
                "foo-bar": {"type": "string"},
                "foo_bar": {"type": "string"}
            },
            "required": ["foo-bar", "foo_bar"]
        }),
        BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        generate_rust(&colliding).unwrap_err().reason(),
        "name_collision"
    );
}

#[test]
fn generated_code_compiles_and_calls_real_core_json_dispatch() {
    let generated = generate_rust(&fixture()).unwrap();
    let dir = TestDirectory::new();
    std::fs::write(dir.path.join("generated.rs"), generated).unwrap();
    let shared: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/contracts/v1.json")).unwrap();
    let shared_contract = Contract::from_json(
        &serde_json::to_string(&shared["descriptors"][0]["descriptor"]).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.path.join("shared.rs"),
        generate_rust(&shared_contract).unwrap(),
    )
    .unwrap();
    let typed_counter =
        Contract::from_json(include_str!("../../../fixtures/contracts/typed-rust.json")).unwrap();
    std::fs::write(
        dir.path.join("typed_counter.rs"),
        generate_rust(&typed_counter).unwrap(),
    )
    .unwrap();
    let remote = Contract::remote(
        "example.generated.variant",
        1,
        json!({"type": "object", "properties": {}}),
        Placement::Named {
            name: "generated-test".into(),
        },
        BTreeMap::from([
            (
                "main".into(),
                BTreeMap::from([(
                    "read".into(),
                    Member::Method {
                        args: None,
                        result: portable(json!({"type": "integer"})),
                    },
                )]),
            ),
            (
                "other".into(),
                BTreeMap::from([(
                    "read".into(),
                    Member::Method {
                        args: None,
                        result: portable(json!({"type": "string"})),
                    },
                )]),
            ),
        ]),
    )
    .unwrap();
    std::fs::write(dir.path.join("remote.rs"), generate_rust(&remote).unwrap()).unwrap();
    let source = include_str!("fixtures/consumer.rs");
    std::fs::write(dir.path.join("main.rs"), source).unwrap();
    compile_and_run(&dir.path, false);
}

#[test]
fn cli_round_trips_descriptor_and_rejects_duplicate_keys() {
    let directory = TestDirectory::new();
    let descriptor = directory.path.join("contract.json");
    let output = directory.path.join("generated.rs");
    std::fs::write(&descriptor, serde_json::to_string(&fixture()).unwrap()).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_tembra-testing-codegen"))
        .args([&descriptor, &output])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        generate_rust(&fixture()).unwrap()
    );
    let invalid =
        serde_json::to_string(&fixture())
            .unwrap()
            .replacen("{", "{\"name\":\"forged\",", 1);
    std::fs::write(&descriptor, invalid).unwrap();
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_tembra-testing-codegen"))
            .arg(&descriptor)
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn optional_presence_is_supported_without_materializing_schema_defaults() {
    for schema in [
        json!({"type": "array", "items": {"type": "integer"}}),
        json!({"type": "array", "items": {"type": "integer"}, "minItems": 1}),
        json!({"anyOf": [{"type": "null"}, {"type": "string"}]}),
        json!({"type": "object", "properties": {}}),
    ] {
        let contract = Contract::local(
            "presence",
            1,
            json!({"type": "object", "properties": {"optional": schema}}),
            BTreeMap::new(),
        )
        .unwrap();
        assert!(generate_rust(&contract).unwrap().contains("Presence<"));
    }
    let contract = Contract::local(
        "defaulted",
        1,
        json!({
            "type": "object",
            "properties": {"name": {"type": "string", "default": "private-default"}}
        }),
        BTreeMap::new(),
    )
    .unwrap();
    let generated = generate_rust(&contract).unwrap();
    assert!(generated.contains("Presence<"));
    assert!(!generated.contains("fn default_name"));
    assert_eq!(
        contract.normalize_input(&json!({})).unwrap(),
        json!({"name": "private-default"})
    );
}

fn native_fixture() -> Contract {
    Contract::local(
        "example.generated.native",
        1,
        json!({"type":"object","properties":{},"additionalProperties":false}),
        BTreeMap::from([
            (
                "identity".into(),
                Member::Method {
                    args: Some(ValueSchema::Native("marker@1".into())),
                    result: Some(ValueSchema::Native("marker@1".into())),
                },
            ),
            (
                "poison".into(),
                Member::Method {
                    args: Some(ValueSchema::Native("poison@1".into())),
                    result: Some(ValueSchema::Native("poison@1".into())),
                },
            ),
            (
                "nativePortable".into(),
                Member::Method {
                    args: portable(json!({"type":"integer"})),
                    result: portable(json!({"type":"integer"})),
                },
            ),
            (
                "inspect".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Native("context@1".into())),
                },
            ),
            (
                "capability".into(),
                Member::Opaque {
                    id: "capability@1".into(),
                },
            ),
            (
                "count".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
        ]),
    )
    .unwrap()
}

#[test]
fn explicit_native_bindings_compile_and_preserve_managed_identity_and_context() {
    let options = GenerationOptions {
        name: Some("Counter".into()),
        native_types: BTreeMap::from([
            ("marker@1".into(), "::std::rc::Rc<crate::Marker>".into()),
            ("poison@1".into(), "crate::PoisonNative".into()),
            ("context@1".into(), "::tembra_core::Context".into()),
            ("capability@1".into(), "::std::cell::Cell<u32>".into()),
        ]),
        native_methods: BTreeMap::from([(
            "nativePortable".into(),
            NativeMethodBinding {
                args: Some("u64".into()),
                result: Some("u64".into()),
            },
        )]),
    };
    let contract = native_fixture();
    let before = contract.clone();
    let generated = generate_rust_with_options(&contract, &options).unwrap();
    assert_eq!(contract, before);
    assert!(!generated.contains(".call_json("));
    assert!(generated.contains("pub use variant_local::Token as Counter"));
    let directory = TestDirectory::new();
    std::fs::write(directory.path.join("generated.rs"), &generated).unwrap();
    let descriptor_path = directory.path.join("native.json");
    let options_path = directory.path.join("bindings.json");
    let cli_output = directory.path.join("from_cli.rs");
    std::fs::write(&descriptor_path, serde_json::to_string(&contract).unwrap()).unwrap();
    std::fs::write(&options_path, serde_json::to_string(&options).unwrap()).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_tembra-testing-codegen"))
        .args([&descriptor_path, &cli_output, &options_path])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(std::fs::read_to_string(&cli_output).unwrap(), generated);
    std::fs::write(&options_path, r#"{"nativeTypes":{},"nativeTypes":{}}"#).unwrap();
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_tembra-testing-codegen"))
            .args([&descriptor_path, &cli_output, &options_path])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(std::fs::read_to_string(&cli_output).unwrap(), generated);
    let portable =
        Contract::from_json(include_str!("../../../fixtures/contracts/typed-rust.json")).unwrap();
    std::fs::write(
        directory.path.join("portable.rs"),
        generate_rust(&portable).unwrap(),
    )
    .unwrap();
    let Member::State { schema } = &portable.members("default").unwrap()["count"] else {
        panic!("portable counter fixture must expose count state");
    };
    let forwarded = Contract::local(
        "example.generated-forwarded",
        1,
        json!({"type":"object","properties":{},"additionalProperties":false}),
        BTreeMap::from([(
            "count".into(),
            Member::State {
                schema: schema.clone(),
            },
        )]),
    )
    .unwrap();
    std::fs::write(
        directory.path.join("forwarded.rs"),
        generate_rust(&forwarded).unwrap(),
    )
    .unwrap();
    let user_contract = Contract::remote(
        "example.generated-user",
        1,
        json!({"type":"object","properties":{},"additionalProperties":false}),
        Placement::Named {
            name: "service".into(),
        },
        BTreeMap::from([(
            "main".into(),
            BTreeMap::from([
                (
                    "user".into(),
                    Member::State {
                        schema: json!({
                            "type":"object", "properties": {
                                "name":{"type":"string"},
                                "age":{"type":"integer","minimum":0,"maximum":100},
                                "note":{"anyOf":[{"type":"null"},{"type":"string"}]}
                            },
                            "required":["name","age"], "additionalProperties":false
                        }),
                    },
                ),
                (
                    "birthday".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                ),
            ]),
        )]),
    )
    .unwrap();
    std::fs::write(
        directory.path.join("custom.rs"),
        generate_rust(&user_contract).unwrap(),
    )
    .unwrap();
    std::fs::write(
        directory.path.join("main.rs"),
        include_str!("fixtures/native_consumer.rs"),
    )
    .unwrap();
    compile_and_run(&directory.path, true);
    let source = include_str!("fixtures/native_consumer.rs");
    for (source, expected) in [
        (
            source.replace(
                "counter.native_portable(4)",
                "counter.native_portable(\"wrong\")",
            ),
            "mismatched types",
        ),
        (
            source.replace("async fn identity(", "async fn missing_identity("),
            "not all trait items implemented",
        ),
        (
            source.replace("number: u64,", "number: String,"),
            "incompatible type for trait",
        ),
    ] {
        std::fs::write(directory.path.join("main.rs"), source).unwrap();
        let result = Command::new(env!("CARGO"))
            .args(["check", "--offline", "--quiet", "--manifest-path"])
            .arg(directory.path.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/codegen-consumer-tests"))
            .output()
            .unwrap();
        assert!(
            !result.status.success(),
            "invalid generated API use unexpectedly compiled"
        );
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn invalid_explicit_bindings_and_public_name_collisions_fail_closed() {
    for options in [
        GenerationOptions {
            native_types: BTreeMap::from([("unknown@1".into(), "u64".into())]),
            ..Default::default()
        },
        GenerationOptions {
            native_types: BTreeMap::from([("marker@1".into(), "u64; fn injected() {}".into())]),
            ..Default::default()
        },
        GenerationOptions {
            native_methods: BTreeMap::from([("missing".into(), NativeMethodBinding::default())]),
            ..Default::default()
        },
        GenerationOptions {
            native_methods: BTreeMap::from([(
                "nativePortable".into(),
                NativeMethodBinding::default(),
            )]),
            ..Default::default()
        },
    ] {
        assert_eq!(
            generate_rust_with_options(&native_fixture(), &options)
                .unwrap_err()
                .reason(),
            "invalid_binding"
        );
    }
    let remote =
        Contract::from_json(include_str!("../../../fixtures/contracts/typed-rust.json")).unwrap();
    assert_eq!(
        generate_rust_with_options(
            &remote,
            &GenerationOptions {
                native_methods: BTreeMap::from([(
                    "increment".into(),
                    NativeMethodBinding {
                        args: Some("u64".into()),
                        result: Some("u64".into())
                    }
                )]),
                ..Default::default()
            }
        )
        .unwrap_err()
        .reason(),
        "invalid_binding"
    );
    for names in [
        ["with_context", "read"],
        ["core", "read"],
        ["clone", "read"],
        ["Clone", "read"],
    ] {
        let contract = Contract::local(
            "generated.collision",
            1,
            json!({"type":"object","properties":{}}),
            names
                .into_iter()
                .map(|name| {
                    (
                        name.to_owned(),
                        Member::Method {
                            args: None,
                            result: None,
                        },
                    )
                })
                .collect(),
        )
        .unwrap();
        assert_eq!(
            generate_rust(&contract).unwrap_err().reason(),
            "name_collision"
        );
    }
}

#[test]
fn method_names_do_not_reserve_removed_call_aliases() {
    let contract = Contract::local(
        "example.call_prefix",
        1,
        json!({"type": "object", "properties": {}}),
        ["read", "call_read"]
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                )
            })
            .collect(),
    )
    .unwrap();
    let generated = generate_rust(&contract).unwrap();
    assert!(generated.contains("pub async fn read("));
    assert!(generated.contains("pub async fn call_read("));
    assert!(!generated.contains("pub async fn call_call_read("));
}

#[test]
fn descriptor_cache_does_not_capture_root_token_aliases() {
    let directory = TestDirectory::new();
    let contract = Contract::local(
        "example.cache_alias",
        1,
        json!({"type": "object", "properties": {}}),
        BTreeMap::new(),
    )
    .unwrap();
    let mut modules = String::new();
    let mut checks = String::new();
    for (index, alias) in [
        "Some",
        "Ok",
        "slot",
        "descriptor",
        "__tembra_descriptor_slot",
        "__tembra_cached_descriptor",
        "__TEMBRA_DESCRIPTOR_CACHE",
        "__tembra_build_descriptor",
        "error",
        "__tembra_descriptor_error",
    ]
    .into_iter()
    .enumerate()
    {
        let options = GenerationOptions {
            name: Some(alias.into()),
            ..GenerationOptions::default()
        };
        let source = generate_rust_with_options(&contract, &options).unwrap();
        std::fs::write(directory.path.join(format!("binding_{index}.rs")), source).unwrap();
        modules.push_str(&format!(
            "mod binding_{index} {{ include!(\"binding_{index}.rs\"); }}\n"
        ));
        checks.push_str(&format!(
            "assert!(std::rc::Rc::ptr_eq(&binding_{index}::contract().unwrap(), \
             &binding_{index}::contract().unwrap()));\n"
        ));
    }
    std::fs::write(
        directory.path.join("main.rs"),
        format!(
            r#"{modules}
            struct Finalizer;
            impl Drop for Finalizer {{
                fn drop(&mut self) {{ binding_0::contract().unwrap(); }}
            }}
            std::thread_local! {{ static FINALIZER: Finalizer = const {{ Finalizer }}; }}
            fn main() {{
                {checks}
                std::thread::spawn(|| {{
                    FINALIZER.with(|_| {{}});
                    binding_0::contract().unwrap();
                }}).join().unwrap();
            }}"#
        ),
    )
    .unwrap();
    compile_and_run(&directory.path, true);
}

fn compile_and_run(directory: &Path, facade_only: bool) {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let core = workspace.join("crates/core");
    let contracts = workspace.join("crates/contracts");
    let facade = workspace.join("crates/tembra");
    let core_dependencies = if facade_only {
        String::new()
    } else {
        format!(
            r#"
tembra-core = {{ path = {:?} }}
tembra-contracts = {{ path = {:?} }}
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
futures = "0.3"
"#,
            core.to_string_lossy(),
            contracts.to_string_lossy()
        )
    };
    let manifest = format!(
        r#"
[workspace]

[package]
name = "generated-consumer-fixture"
version = "0.0.0"
edition = "2024"

[[bin]]
name = "consumer"
path = "main.rs"

[dependencies]
tembra = {{ path = {:?} }}
{core_dependencies}
"#,
        facade.to_string_lossy()
    );
    std::fs::write(directory.join("Cargo.toml"), manifest).unwrap();
    let result = Command::new(env!("CARGO"))
        .args(["run", "--offline", "--quiet", "--manifest-path"])
        .arg(directory.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(workspace.join("target/codegen-consumer-tests"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "generated consumer failed to compile or execute:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

struct TestDirectory {
    path: PathBuf,
}
impl TestDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tembra-codegen-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
