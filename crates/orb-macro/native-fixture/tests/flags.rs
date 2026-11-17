//! A module consuming an interface that holds `flags`, in a crate that depends on the facade alone:
//! the runtime fixture's `holder`, built natively with the facade's `host`, whose `wit-bindgen`
//! lacks the `bitflags` feature this crate enables for its own `keeper`.
use std::{path::Path, process::Command};

/// The runtime fixture's `holder` builds natively with the facade alone, and its native test
/// calls the implemented interface and the derived service. The build goes to this crate's
/// scratch directory.
#[test]
#[ignore = "fails on main: the native codec of a consumed flags calls contains and set, which wit-bindgen's own bitflags! lacks; fixed in #10"]
fn a_consumed_flags_builds_natively_with_the_facade_alone() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../runtime-fixture/Cargo.toml");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("runtime-fixture");
    let output = Command::new(env!("CARGO"))
        .args(["test", "--locked", "--lib", "--no-default-features"])
        .args(["--features", "flagged,native"])
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
