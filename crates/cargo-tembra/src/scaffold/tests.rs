//! The generated guest uses the facade's macros without linking the host implementation.
use super::{manifest, without_verbatim_prefix};
use std::path::PathBuf;

#[test]
fn the_facade_path_is_written_as_a_toml_string_without_host_features() {
    // An ordinary path reads as it did; quotes and backslashes are escaped and read back.
    let path = "/home/ada/tembra/crates/tembra";
    for path in [
        path,
        r#"/home/ada/a "quoted" \ dir\tembra/crates/tembra"#,
        r"C:\Users\ada\tembra\crates\tembra",
    ] {
        let manifest: toml::Value = toml::from_str(&manifest("greeter", path)).unwrap();
        assert_eq!(
            manifest["dependencies"]["tembra"]["path"].as_str(),
            Some(path)
        );
        assert_eq!(
            manifest["dependencies"]["tembra"]["default-features"].as_bool(),
            Some(false)
        );
        assert_eq!(
            manifest["dependencies"]["tembra"]["features"][0].as_str(),
            Some("macros")
        );
    }
}

#[test]
fn a_verbatim_drive_path_loses_its_prefix() {
    let stripped = |path: &str| without_verbatim_prefix(PathBuf::from(path));
    assert_eq!(
        stripped(r"\\?\C:\Users\ada\tembra"),
        PathBuf::from(r"C:\Users\ada\tembra")
    );
    // A verbatim UNC or volume path, and an ordinary path, stay as they are.
    for path in [
        r"\\?\UNC\server\share\tembra",
        r"\\?\Volume{8e1c}\tembra",
        "/home/ada/tembra",
    ] {
        assert_eq!(stripped(path), PathBuf::from(path));
    }
}
