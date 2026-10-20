//! Compile-time acceptance uses the actual built facade and streams compiler output in memory.

use std::{env, fs, path::PathBuf, process::Command};

#[test]
fn typed_contract_boundaries_accept_valid_rust_and_reject_invalid_programs() {
    let dependencies = env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let library = fs::read_dir(&dependencies)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libtembra-") && name.ends_with(".rlib"))
        })
        .max_by_key(|path| fs::metadata(path).unwrap().modified().unwrap())
        .expect("the integration test has a built tembra library");
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/compile");
    for (fixture, expected) in [
        ("valid.rs", None),
        ("documented_shadowed_prelude.rs", None),
        (
            "missing_scope.rs",
            Some("contract requires exactly one explicit scope: local or remote"),
        ),
        (
            "contradictory_scope.rs",
            Some("duplicate or conflicting contract option"),
        ),
        (
            "repeated_scope.rs",
            Some("duplicate or conflicting contract option"),
        ),
        ("explicit_scope.rs", None),
        ("async_factory.rs", Some("mismatched types")),
        ("non_provider.rs", Some("CounterProvider")),
        (
            "clone_member.rs",
            Some("member name is reserved by the typed reference API"),
        ),
        (
            "duplicate_options.rs",
            Some("duplicate or conflicting contract option"),
        ),
        (
            "malformed_helper.rs",
            Some("state does not accept arguments"),
        ),
        (
            "missing_opaque_id.rs",
            Some("opaque requires a nonempty id"),
        ),
        (
            "duplicate_helpers.rs",
            Some("duplicate contract member attribute"),
        ),
        (
            "conflicting_helpers.rs",
            Some("state/opaque getters cannot use portable"),
        ),
        (
            "reserved_member.rs",
            Some("member name is reserved by the typed reference API"),
        ),
        ("wrong_argument.rs", Some("mismatched types")),
        ("wrong_input.rs", Some("mismatched types")),
        (
            "missing_provider_method.rs",
            Some("not all trait items implemented"),
        ),
        (
            "wrong_provider_signature.rs",
            Some("incompatible type for trait"),
        ),
        (
            "async_state.rs",
            Some("state/opaque getters must be synchronous"),
        ),
        (
            "generic_contract.rs",
            Some("contract traits cannot have generics"),
        ),
        (
            "remote_opaque.rs",
            Some("remote contracts cannot expose opaque"),
        ),
        (
            "unsupported_attribute.rs",
            Some("unsupported contract member attribute"),
        ),
    ] {
        let result = Command::new("rustc")
            .args([
                "--edition=2024",
                "--crate-type=lib",
                "--emit=metadata",
                "-o",
                "-",
            ])
            .arg("--extern")
            .arg(format!("tembra={}", library.display()))
            .arg("-L")
            .arg(format!("dependency={}", dependencies.display()))
            .arg(fixtures.join(fixture))
            .output()
            .expect("rustc must be available for compile-time acceptance");
        let diagnostics = String::from_utf8_lossy(&result.stderr);
        if let Some(expected) = expected {
            assert!(!result.status.success(), "{fixture} unexpectedly compiled");
            assert!(diagnostics.contains(expected), "{fixture}: {diagnostics}");
            assert!(
                !diagnostics.contains("E0432") && !diagnostics.contains("E0433"),
                "fixture must fail at its intended type boundary: {fixture}: {diagnostics}"
            );
        } else {
            assert!(result.status.success(), "{fixture}: {diagnostics}");
        }
    }
}
