//! Callable authority follows a world's own imports, never type-only elaboration (Phase 3A).
use crate::{Contract, FilesystemGrant, Grants, NetworkGrant, PreparedContract, SystemCapability};
use std::collections::{BTreeMap, BTreeSet};

const FS: &str = "import wasi:filesystem/types@0.3.0; import wasi:filesystem/preopens@0.3.0;";

fn grants(clocks: bool, filesystem: bool) -> Grants {
    Grants {
        filesystem: filesystem
            .then(|| FilesystemGrant {
                provider: "data".into(),
                mount: "/data".into(),
                writable: false,
            })
            .into_iter()
            .collect(),
        system: if clocks {
            [SystemCapability::Clocks].into()
        } else {
            BTreeSet::new()
        },
        ..Grants::default()
    }
}

/// Run the real admission chain: provider selection, plan, grant check. Returns HostImports.
fn admit(contract: &Contract, grants: &Grants) -> Result<Vec<String>, String> {
    let bindings = contract
        .providers(&BTreeMap::new(), grants)
        .map_err(|e| e.to_string())?;
    let plan = contract
        .plan("orb", 1, &bindings)
        .map_err(|e| e.to_string())?;
    grants
        .check(
            &plan.host_imports,
            &["data".to_owned()].into(),
            &["net".to_owned()].into(),
        )
        .map_err(|e| e.to_string())?;
    Ok(plan
        .host_imports
        .iter()
        .map(|i| format!("{}={}", i.binding, i.contract))
        .collect())
}

fn source(text: &str) -> Contract {
    Contract::from_source("probe.wit", text, Some("app")).unwrap()
}

/// `(type_only, explicit)` of one described import.
fn flags(contract: &Contract, binding: &str) -> (bool, bool) {
    let description = contract.describe();
    let item = description["imports"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["binding"] == binding)
        .unwrap_or_else(|| panic!("{binding} is not imported"));
    (
        item["type_only"].as_bool().unwrap(),
        item["explicit"].as_bool().unwrap(),
    )
}

const CLOCK: &str = "wasi:clocks/system-clock@0.3.0";

#[test]
fn filesystem_only_needs_no_clock_grant_and_keeps_clock_types() {
    let contract = source(&format!("package probe:one; world app {{ {FS} }}"));
    let imports = admit(&contract, &grants(false, true)).unwrap();
    assert!(
        imports.iter().all(|import| !import.contains("wasi:clocks")),
        "{imports:?}"
    );
    // The dependency stays in the model for type identity (stat timestamps), without authority.
    assert_eq!(flags(&contract, CLOCK), (true, false));
    assert_eq!(
        flags(&contract, "wasi:filesystem/types@0.3.0"),
        (false, true)
    );
    let types = contract.describe()["types"].to_string();
    assert!(types.contains("seconds") && types.contains("nanoseconds"));
}

#[test]
fn explicit_included_and_aliased_clocks_require_the_grant() {
    for (name, world) in [
        (
            "explicit",
            format!("package probe:two; world app {{ {FS} import {CLOCK}; }}"),
        ),
        (
            "local include",
            format!(
                "package probe:three; world base {{ import {CLOCK}; }} world app {{ include base; {FS} }}"
            ),
        ),
        (
            "foreign include",
            format!("package probe:four; world app {{ include wasi:clocks/imports@0.3.0; {FS} }}"),
        ),
        (
            "named alias",
            format!("package probe:five; world app {{ import wall: {CLOCK}; {FS} }}"),
        ),
    ] {
        let contract = source(&world);
        let refused = admit(&contract, &grants(false, true)).unwrap_err();
        assert!(refused.contains("grant"), "{name}: {refused}");
        let imports = admit(&contract, &grants(true, true)).unwrap();
        assert!(
            imports.iter().any(|import| import.ends_with(CLOCK)),
            "{name}: {imports:?}"
        );
        // The unstable timezone interface of `wasi:clocks/imports` is never admitted.
        assert!(
            imports.iter().all(|import| !import.contains("timezone")),
            "{name}"
        );
    }
    // The named alias is the authority; the elaborated dependency key stays type-only.
    let alias = source(&format!(
        "package probe:six; world app {{ import wall: {CLOCK}; {FS} }}"
    ));
    assert_eq!(flags(&alias, "wall"), (false, true));
    assert_eq!(flags(&alias, CLOCK), (true, false));
}

#[test]
fn export_and_world_level_type_uses_need_no_clock_grant() {
    let export = source(
        "package probe:seven; interface stamps { use wasi:clocks/system-clock@0.3.0.{instant}; get: func() -> instant; } world app { export stamps; }",
    );
    assert!(admit(&export, &grants(false, false)).unwrap().is_empty());
    let used = source(&format!(
        "package probe:eight; world app {{ use {CLOCK}.{{instant}}; {FS} }}"
    ));
    assert!(admit(&used, &grants(false, true)).is_ok());
    assert_eq!(flags(&used, CLOCK), (true, false));
}

#[test]
fn directory_dependencies_keep_provenance() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("deps")).unwrap();
    std::fs::write(
        directory.path().join("deps/base.wit"),
        format!("package probe:base; world clocked {{ import {CLOCK}; }}"),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("app.wit"),
        format!("package probe:dir; world app {{ include probe:base/clocked; {FS} }} world plain {{ {FS} }}"),
    )
    .unwrap();
    let included = Contract::from_dir(directory.path(), Some("app")).unwrap();
    assert!(admit(&included, &grants(false, true)).is_err());
    assert!(admit(&included, &grants(true, true)).is_ok());
    let plain = Contract::from_dir(directory.path(), Some("plain")).unwrap();
    assert!(admit(&plain, &grants(false, true)).is_ok());
}

#[test]
fn provenance_survives_the_prepared_codec_but_not_binary_wit() {
    let contract = source(&format!("package probe:nine; world app {{ {FS} }}"));
    let decoded = PreparedContract::decode(&contract.prepared().encode()).unwrap();
    assert_eq!(&decoded, contract.prepared());
    // An encoded WIT package has no provenance: every import is conservatively explicit.
    let binary = Contract::from_bytes(&contract.encode().unwrap(), Some("app")).unwrap();
    assert_eq!(flags(&binary, CLOCK), (false, true));
    assert!(admit(&binary, &grants(false, true)).is_err());
}

#[test]
fn compiled_binary_imports_require_grants_by_interface_identity() {
    let none = grants(false, false);
    assert!(none.check_component_import(CLOCK).is_err());
    assert!(
        none.check_component_import("wasi:clocks/system-clock@0.3.9")
            .is_err()
    );
    assert!(grants(true, false).check_component_import(CLOCK).is_ok());
    assert!(
        none.check_component_import("wasi:filesystem/types@0.3.0")
            .is_err()
    );
    assert!(
        grants(false, true)
            .check_component_import("wasi:filesystem/preopens@0.3.0")
            .is_ok()
    );
    assert!(
        none.check_component_import("wasi:clocks/timezone@0.3.0")
            .is_err()
    );
}

/// Write `files` (relative path, contents) under a fresh WIT directory.
fn directory(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (name, contents) in files {
        let file = root.path().join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, contents).unwrap();
    }
    root
}

fn requires_clocks(contract: &Contract) -> bool {
    // A filesystem grant needs a preopens import, so worlds without one are admitted without it.
    let filesystem = contract
        .describe()
        .to_string()
        .contains("wasi:filesystem/preopens");
    admit(contract, &grants(false, filesystem)).is_err()
        && admit(contract, &grants(true, filesystem)).is_ok()
}

#[test]
fn qualified_world_selection_uses_that_worlds_own_provenance() {
    // Equal bare names in main, nested, `deps/` and bundled standard packages.
    let main = format!(
        "package probe:main; package probe:inner {{ world clocked {{ import {CLOCK}; {FS} }} }} world clocked {{ {FS} }} world imports {{ {FS} }}"
    );
    let deps = format!("package probe:base; world clocked {{ import {CLOCK}; {FS} }}");
    let root = directory(&[
        ("app.wit", main.as_bytes()),
        ("deps/base.wit", deps.as_bytes()),
    ]);
    let open = |world: &str| Contract::from_dir(root.path(), Some(world)).unwrap();
    for world in ["clocked", "probe:main/clocked", "imports"] {
        let contract = open(world);
        assert!(admit(&contract, &grants(false, true)).is_ok(), "{world}");
        assert_eq!(flags(&contract, CLOCK), (true, false), "{world}");
    }
    for world in [
        "probe:base/clocked",
        "probe:inner/clocked",
        "wasi:clocks/imports@0.3.0",
    ] {
        let contract = open(world);
        assert!(requires_clocks(&contract), "{world}");
        assert_eq!(flags(&contract, CLOCK), (false, true), "{world}");
    }
    // The same through a single source.
    let nested = Contract::from_source("probe.wit", &main, Some("probe:inner/clocked")).unwrap();
    assert!(requires_clocks(&nested));
}

#[test]
fn gated_imports_and_includes_do_not_authorize_their_type_only_dependency() {
    for (name, world) in [
        (
            "gated import",
            format!(
                "package probe:gated; world app {{ @unstable(feature = clocky) import {CLOCK}; {FS} }}"
            ),
        ),
        (
            "gated include",
            format!(
                "package probe:gated; world base {{ import {CLOCK}; }} world app {{ @unstable(feature = clocky) include base; {FS} }}"
            ),
        ),
    ] {
        let contract = source(&world);
        // Filesystem elaboration still keeps system-clock for `instant`, without authority.
        assert_eq!(flags(&contract, CLOCK), (true, false), "{name}");
        assert!(admit(&contract, &grants(false, true)).is_ok(), "{name}");
    }
    // A released `@since` gate is active and stays explicit.
    let since = source(&format!(
        "package probe:since@1.0.0; world app {{ @since(version = 1.0.0) import {CLOCK}; {FS} }}"
    ));
    assert!(requires_clocks(&since));
}

#[test]
fn include_renames_keep_the_alias_explicit() {
    let contract = source(&format!(
        "package probe:rename; world base {{ import wall: {CLOCK}; }} world app {{ include base with {{ wall as clock }} {FS} }}"
    ));
    assert_eq!(flags(&contract, "clock"), (false, true));
    assert_eq!(flags(&contract, CLOCK), (true, false));
    assert!(requires_clocks(&contract));
}

#[test]
fn encoded_dependencies_have_conservative_provenance_even_named_wit() {
    let encoded = Contract::from_source(
        "base.wit",
        &format!("package probe:base; world clocked {{ import {CLOCK}; }}"),
        Some("clocked"),
    )
    .unwrap()
    .encode()
    .unwrap();
    let main = format!(
        "package probe:encoded; world app {{ include probe:base/clocked; {FS} }} world plain {{ {FS} }}"
    );
    for file in ["deps/base.wasm", "deps/base.wit"] {
        let root = directory(&[("app.wit", main.as_bytes()), (file, &encoded)]);
        // An include into an encoded package has unknown provenance: every import is explicit.
        let included = Contract::from_dir(root.path(), Some("app")).unwrap();
        assert_eq!(flags(&included, CLOCK), (false, true), "{file}");
        assert!(requires_clocks(&included), "{file}");
        // Worlds that do not reach the encoded package keep exact provenance.
        let plain = Contract::from_dir(root.path(), Some("plain")).unwrap();
        assert_eq!(flags(&plain, CLOCK), (true, false), "{file}");
        assert!(admit(&plain, &grants(false, true)).is_ok(), "{file}");
    }
}

#[test]
fn dependency_errors_name_the_dependency_and_duplicates_are_refused() {
    let main = format!("package probe:errors; world app {{ {FS} }}");
    let broken = directory(&[
        ("app.wit", main.as_bytes()),
        ("deps/broken.wit", b"package probe:broken; world {"),
    ]);
    let error = Contract::from_dir(broken.path(), Some("app"))
        .err()
        .unwrap()
        .0;
    assert!(
        error.contains(&broken.path().join("deps/broken.wit").display().to_string()),
        "{error}"
    );
    let folder = directory(&[
        ("app.wit", main.as_bytes()),
        ("deps/folder/x.wit", b"package probe:folder; world {"),
    ]);
    let error = Contract::from_dir(folder.path(), Some("app"))
        .err()
        .unwrap()
        .0;
    assert!(
        error.contains(&folder.path().join("deps/folder").display().to_string()),
        "{error}"
    );
    // Vendoring a bundled standard package is a typed refusal, not a resolver panic.
    let vendored = directory(&[
        ("app.wit", main.as_bytes()),
        (
            "deps/clocks.wit",
            b"package wasi:clocks@0.3.0; interface system-clock { now: func() -> u64; }",
        ),
    ]);
    let error = Contract::from_dir(vendored.path(), Some("app"))
        .err()
        .unwrap()
        .0;
    assert!(
        error.contains("wasi:clocks@0.3.0") && error.contains("bundled"),
        "{error}"
    );
    let error = Contract::from_source(
        "dup.wit",
        "package probe:dup; package wasi:random@0.3.0 { interface random { get: func() -> u64; } } world app {}",
        Some("app"),
    )
    .err()
    .unwrap()
    .0;
    assert!(error.contains("wasi:random@0.3.0"), "{error}");
}

#[test]
fn resource_dependency_interfaces_stay_callable_and_network_bound() {
    // Only name lookup is imported; `wasi:sockets/types` is an elaborated dependency that owns
    // resources (with `create` statics), so it stays callable and needs the network grant.
    let contract =
        source("package probe:lookup; world app { import wasi:sockets/ip-name-lookup@0.3.0; }");
    assert_eq!(flags(&contract, "wasi:sockets/types@0.3.0"), (false, false));
    assert!(admit(&contract, &Grants::default()).is_err());
    let network = Grants {
        network: vec![NetworkGrant {
            provider: "net".into(),
            resolve: true,
            ..NetworkGrant::default()
        }],
        ..Grants::default()
    };
    let imports = admit(&contract, &network).unwrap();
    assert!(
        imports
            .iter()
            .any(|import| import.ends_with("wasi:sockets/types@0.3.0"))
            && imports
                .iter()
                .any(|import| import.ends_with("wasi:sockets/ip-name-lookup@0.3.0")),
        "{imports:?}"
    );
}
