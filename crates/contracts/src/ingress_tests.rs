//! The exact incoming HTTP export is recognized, kept out of the portable profile, and admitted
//! without opening outgoing HTTP authority.
use super::*;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use tembra_core::Code;

fn contract(world: &str) -> Contract {
    let source = format!("package example:ingress@1.0.0;\n{world}");
    Contract::from_source("ingress.wit", &source, Some("app")).unwrap()
}

/// A world that only serves incoming HTTP.
fn handler_only() -> Contract {
    contract("world app { export wasi:http/handler@0.3.0; }")
}

fn http_grant(provider: &str) -> Grants {
    Grants {
        http: vec![HttpGrant {
            provider: provider.into(),
            origins: vec!["https://example.com".into()],
        }],
        ..Grants::default()
    }
}

#[test]
fn the_exact_handler_export_is_ingress_and_not_a_portable_service() {
    let prepared = handler_only().prepared().clone();
    assert_eq!(prepared.ingress(), Some(HTTP_HANDLER));
    // Neither a service, a value-profile slot nor a signature is published for it.
    prepared.check_value_profile().unwrap();
    let bindings = prepared
        .providers(&BTreeMap::new(), &Grants::default())
        .unwrap();
    let plan = prepared.plan("web", 1, &bindings).unwrap();
    assert_eq!(plan.ingress.as_deref(), Some(HTTP_HANDLER));
    assert!(plan.orb.services.is_empty(), "{:?}", plan.orb.services);
    assert_eq!(plan.describe()["ingress"], HTTP_HANDLER);
    assert!(prepared.bind(HTTP_HANDLER, "handle", true).is_ok());
    // The handler implementation is consumed by the ingress check, never returned as a service.
    let services = prepared
        .check_implementation(
            "web",
            1,
            implementation(json!({ "handler": { "methods": ["handle"] } })),
        )
        .unwrap();
    assert!(services.is_empty());
}

fn implementation(services: serde_json::Value) -> Implementation {
    serde_json::from_value(json!({ "services": services })).unwrap()
}

#[test]
fn handler_implementations_declare_exactly_the_handle_method() {
    let prepared = handler_only().prepared().clone();
    let binding = prepared
        .check_ingress(
            "web",
            &implementation(
                json!({ "handler": { "methods": ["handle"], "lifetime": "host", "linger": 5 } }),
            ),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        binding,
        IngressBinding {
            name: "web::wasi:http/handler@0.3.0".into(),
            interface: HTTP_HANDLER.into(),
            alias: "handler".into(),
            method: "handle".into(),
            lifetime: Lifetime::Host,
            linger: 5,
        }
    );
    for (services, expected) in [
        (json!({}), "missing service implementation handler"),
        (json!({ "handler": { "methods": [] } }), "exactly"),
        (
            json!({ "handler": { "methods": ["handle", "extra"] } }),
            "exactly",
        ),
        (json!({ "handler": { "methods": ["serve"] } }), "exactly"),
        (
            json!({ "handler": { "methods": ["handle"], "access": true } }),
            "access policy",
        ),
    ] {
        let error = prepared
            .check_implementation("web", 1, implementation(services))
            .unwrap_err();
        assert!(error.0.contains(expected), "{error}");
    }
    // An extra service beside the handler is still absent from the world.
    let error = prepared
        .check_implementation(
            "web",
            1,
            implementation(
                json!({ "handler": { "methods": ["handle"] }, "other": { "methods": [] } }),
            ),
        )
        .unwrap_err();
    assert!(error.0.contains("absent from its WIT world"), "{error}");
    // Without the export, a `handler` service is an ordinary extra.
    let plain = contract("world app { import wasi:http/types@0.3.0; }");
    assert_eq!(
        plain
            .prepared()
            .check_ingress(
                "web",
                &implementation(json!({ "handler": { "methods": ["handle"] } }))
            )
            .unwrap(),
        None
    );
}

#[test]
fn a_handler_alias_shared_with_another_export_is_ambiguous() {
    let ambiguous = contract(
        "interface handler { ping: func() -> u32; }
         world app { export handler; export wasi:http/handler@0.3.0; }",
    );
    let error = ambiguous
        .prepared()
        .check_ingress(
            "web",
            &implementation(json!({ "handler": { "methods": ["handle"] } })),
        )
        .unwrap_err();
    assert!(error.0.contains("ambiguous"), "{error}");
}

#[test]
fn other_resource_exports_still_require_a_lifecycle_adapter() {
    let other = contract(
        "interface files { use wasi:filesystem/types@0.3.0.{descriptor}; open: func() -> descriptor; }
         world app { export files; }",
    );
    let prepared = other.prepared();
    assert_eq!(prepared.ingress(), None);
    let bindings = prepared
        .providers(&BTreeMap::new(), &Grants::default())
        .unwrap_or_default();
    let error = prepared.plan("web", 1, &bindings).unwrap_err();
    assert!(error.0.contains("lifecycle adapter"), "{error}");
}

#[test]
fn handler_only_worlds_install_without_any_grant() {
    let prepared = handler_only().prepared().clone();
    let bindings = prepared
        .providers(&BTreeMap::new(), &Grants::default())
        .unwrap();
    assert_eq!(
        bindings.get("wasi:http/types@0.3.0"),
        Some(&Binding::Host {
            provider: lower::INGRESS_PROVIDER.into()
        })
    );
    let plan = prepared.plan("web", 1, &bindings).unwrap();
    Grants::default()
        .check(
            &plan.host_imports,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();
    // The binary check admits the types only for a binary that serves the handler.
    let grants = Grants::default();
    grants
        .check_component_import("wasi:http/types@0.3.0", true)
        .unwrap();
    let refused = grants
        .check_component_import("wasi:http/types@0.3.0", false)
        .unwrap_err();
    assert_eq!(refused.code, Code::Forbidden);
}

#[test]
fn serving_http_never_opens_the_outgoing_client() {
    let both =
        contract("world app { import wasi:http/client@0.3.0; export wasi:http/handler@0.3.0; }");
    let prepared = both.prepared().clone();
    let error = prepared
        .providers(&BTreeMap::new(), &Grants::default())
        .unwrap_err();
    assert_eq!(error.code, Code::Forbidden, "{error}");
    let refused = Grants::default()
        .check_component_import("wasi:http/client@0.3.0", true)
        .unwrap_err();
    assert_eq!(refused.code, Code::Forbidden);
    // With a grant, both interfaces bind to the granted provider as before.
    let bindings = prepared
        .providers(&BTreeMap::new(), &http_grant("web"))
        .unwrap();
    for binding in ["wasi:http/client@0.3.0", "wasi:http/types@0.3.0"] {
        assert_eq!(
            bindings.get(binding),
            Some(&Binding::Host {
                provider: "web".into()
            })
        );
    }
    prepared.plan("web", 1, &bindings).unwrap();
}

#[test]
fn the_ingress_provider_is_refused_outside_a_handler_world() {
    let client = contract("world app { import wasi:http/client@0.3.0; }");
    let prepared = client.prepared();
    // A forged binding cannot reach the client or a types import of a non-handler world.
    for binding in ["wasi:http/client@0.3.0", "wasi:http/types@0.3.0"] {
        let mut bindings = prepared
            .providers(&BTreeMap::new(), &http_grant("web"))
            .unwrap();
        bindings.insert(
            binding.into(),
            Binding::Host {
                provider: lower::INGRESS_PROVIDER.into(),
            },
        );
        let error = prepared.plan("web", 1, &bindings).unwrap_err();
        assert!(error.0.contains("incoming HTTP provider"), "{error}");
    }
    // Nor can it stand in for the client grant of a handler world.
    let both =
        contract("world app { import wasi:http/client@0.3.0; export wasi:http/handler@0.3.0; }");
    let mut bindings = BTreeMap::new();
    for binding in ["wasi:http/client@0.3.0", "wasi:http/types@0.3.0"] {
        bindings.insert(
            binding.to_owned(),
            Binding::Host {
                provider: lower::INGRESS_PROVIDER.into(),
            },
        );
    }
    let error = both.prepared().plan("web", 1, &bindings).unwrap_err();
    assert!(error.0.contains("incoming HTTP provider"), "{error}");
    let error = Grants::default()
        .check(
            &[HostImport {
                binding: "wasi:http/client@0.3.0".into(),
                contract: "wasi:http/client@0.3.0".into(),
                provider: lower::INGRESS_PROVIDER.into(),
            }],
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap_err();
    assert_eq!(error.code, Code::Forbidden);
}

#[test]
fn contract_queries_plan_and_check_the_handler_for_every_host() {
    let prepared = handler_only().prepared().clone();
    let install =
        prepared.request(&json!({"kind": "install", "name": "web", "apiVersion": 1}).to_string());
    let install: serde_json::Value = serde_json::from_str(&install).unwrap();
    assert_eq!(install["ok"]["plan"]["ingress"], HTTP_HANDLER, "{install}");
    assert_eq!(
        install["ok"]["bindings"]["wasi:http/types@0.3.0"]["provider"],
        lower::INGRESS_PROVIDER
    );
    let implementation = prepared.request(
        &json!({"kind": "implementation", "name": "web", "apiVersion": 1, "value": {"services": {"handler": {"methods": ["handle"]}}}, "bindings": install["ok"]["bindings"]})
            .to_string(),
    );
    let implementation: serde_json::Value = serde_json::from_str(&implementation).unwrap();
    let checked = &implementation["ok"];
    assert_eq!(checked["services"], json!([]), "{implementation}");
    assert_eq!(
        checked["ingress"],
        json!({"name": "web::wasi:http/handler@0.3.0", "interface": HTTP_HANDLER, "alias": "handler", "method": "handle", "lifetime": "referenced", "linger": 0})
    );
    let refused = prepared.request(
        &json!({"kind": "implementation", "name": "web", "apiVersion": 1, "value": {"services": {}}})
            .to_string(),
    );
    assert!(
        refused.contains("missing service implementation handler"),
        "{refused}"
    );
}

#[test]
fn a_look_alike_interface_is_not_the_standard_handler() {
    let custom = contract(
        "interface handler { handle: async func(path: string) -> string; }
         world app { export handler; }",
    );
    assert_eq!(custom.prepared().ingress(), None);
}

#[test]
fn installers_cannot_claim_the_reserved_provider_name() {
    let reserved = BTreeSet::from([lower::INGRESS_PROVIDER.to_owned()]);
    for grants in [
        http_grant(lower::INGRESS_PROVIDER),
        Grants {
            network: vec![NetworkGrant {
                provider: lower::INGRESS_PROVIDER.into(),
                ..NetworkGrant::default()
            }],
            ..Grants::default()
        },
        Grants {
            filesystem: vec![FilesystemGrant {
                provider: lower::INGRESS_PROVIDER.into(),
                mount: "/data".into(),
                writable: false,
            }],
            ..Grants::default()
        },
    ] {
        let error = grants
            .check(&[], &reserved, &reserved, &reserved)
            .unwrap_err();
        assert_eq!(error.reason, "host_provider", "{error}");
        assert!(error.message.contains("reserved"), "{error}");
    }
    // Through the full install query too, for a handler world.
    let prepared = handler_only().prepared().clone();
    let bindings = prepared
        .providers(&BTreeMap::new(), &http_grant(lower::INGRESS_PROVIDER))
        .unwrap();
    let plan = prepared.plan("web", 1, &bindings).unwrap();
    let error = http_grant(lower::INGRESS_PROVIDER)
        .check(&plan.host_imports, &reserved, &reserved, &reserved)
        .unwrap_err();
    assert!(error.message.contains("reserved"), "{error}");
}

#[test]
fn types_without_a_handler_export_still_need_an_http_grant() {
    let types = contract(
        "interface util { use wasi:http/types@0.3.0.{fields}; make: func() -> fields; }
         world app { import util; import wasi:http/types@0.3.0; }",
    );
    let error = types
        .prepared()
        .providers(&BTreeMap::new(), &Grants::default())
        .unwrap_err();
    assert_eq!(error.code, Code::Forbidden, "{error}");
}
