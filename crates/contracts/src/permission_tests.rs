use super::*;
use std::collections::BTreeSet;

fn import(contract: &str, provider: &str) -> HostImport {
    HostImport {
        binding: contract.into(),
        contract: contract.into(),
        provider: provider.into(),
    }
}

#[test]
fn declared_imports_never_supply_their_own_authority() {
    for (contract, provider) in [
        ("wasi:filesystem/types@0.3.0", "files"),
        ("wasi:filesystem/preopens@0.3.0", "files"),
        ("wasi:sockets/types@0.3.0", "tcp"),
    ] {
        let error = Grants::default()
            .check(
                &[import(contract, provider)],
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .unwrap_err();
        assert_eq!(error.code, tembra_core::Code::Forbidden);
        assert_eq!(error.reason, "orb_grant");
    }
    assert!(
        Grants::default()
            .check(
                &[import("wasi:clocks/system-clock@0.3.0", "system-clock")],
                &BTreeSet::new(),
                &BTreeSet::new()
            )
            .is_ok()
    );
    let error = Grants::default()
        .check(
            &[import("wasi:http/types@0.3.0", "http")],
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap_err();
    assert_eq!(error.code, tembra_core::Code::Unsupported);
    assert_eq!(error.reason, "wasi_contract");
}

#[test]
fn filesystem_grants_require_real_providers_imports_and_distinct_normalized_mounts() {
    let providers = BTreeSet::from(["files".into()]);
    let imports = [import("wasi:filesystem/preopens@0.3.0", "files")];
    let grant = FilesystemGrant {
        provider: "files".into(),
        mount: "/data".into(),
        writable: false,
    };
    let mut grants = Grants {
        filesystem: vec![grant.clone()],
        ..Grants::default()
    };
    assert!(grants.check(&imports, &providers, &BTreeSet::new()).is_ok());
    assert_eq!(
        grants
            .check(&imports, &BTreeSet::new(), &BTreeSet::new())
            .unwrap_err()
            .reason,
        "host_provider"
    );
    assert_eq!(
        grants
            .check(&[], &providers, &BTreeSet::new())
            .unwrap_err()
            .reason,
        "wit_import"
    );
    for mount in [
        "data",
        "/",
        "/data/",
        "/data/../secret",
        "/data/./secret",
        "/data//secret",
    ] {
        grants.filesystem[0].mount = mount.into();
        assert_eq!(
            grants
                .check(&imports, &providers, &BTreeSet::new())
                .unwrap_err()
                .reason,
            "filesystem_mount",
            "{mount}"
        );
    }
    grants.filesystem = vec![grant.clone(), grant];
    assert_eq!(
        grants
            .check(&imports, &providers, &BTreeSet::new())
            .unwrap_err()
            .reason,
        "filesystem_mount"
    );
}

#[test]
fn network_grants_are_checked_against_the_selected_import_and_provider() {
    let providers = BTreeSet::from(["tcp".into()]);
    let imports = [import("wasi:sockets/types@0.3.0", "tcp")];
    let grants = Grants {
        network: vec![NetworkGrant {
            provider: "tcp".into(),
            connect: vec!["127.0.0.1:8080".parse().unwrap()],
        }],
        ..Grants::default()
    };
    assert!(grants.check(&imports, &BTreeSet::new(), &providers).is_ok());
    assert_eq!(
        grants
            .check(&imports, &BTreeSet::new(), &BTreeSet::new())
            .unwrap_err()
            .reason,
        "host_provider"
    );
    assert_eq!(
        grants
            .check(&[], &BTreeSet::new(), &providers)
            .unwrap_err()
            .reason,
        "wit_import"
    );
}

#[test]
fn metadata_requests_reject_unknown_control_fields_and_preserve_error_codes() {
    let contract =
        Contract::from_source("empty.wit", "package example:empty; world empty {}", None).unwrap();
    for packet in [
        r#"{"kind":"describe","source":"unexpected"}"#,
        r#"{"kind":"bindings","requires":{},"grants":{"network":[],"allowEverything":true}}"#,
    ] {
        let response: serde_json::Value = serde_json::from_str(&contract.request(packet)).unwrap();
        assert_eq!(response["error"]["code"], "invalid");
        assert_eq!(response["error"]["reason"], "wit_contract");
    }
}
