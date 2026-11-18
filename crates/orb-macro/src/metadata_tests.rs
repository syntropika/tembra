//! `[package.metadata.tembra]` read from a crate manifest, with the defaults of `cargo tembra
//! build`.
use super::{Options, expand, metadata::Metadata, tests::literal};
use syn::{Item, ItemMod};

/// A crate manifest under the system's temporary directory.
fn manifest(name: &str, text: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tembra-orb-macro-metadata-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("Cargo.toml");
    std::fs::write(&path, text).unwrap();
    path
}

const PACKAGE: &str =
    "[package]\nname = \"example_greeter\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

fn read(name: &str, text: &str) -> Metadata {
    Metadata::from_manifest(&manifest(name, text)).unwrap_or_else(|error| panic!("{error}"))
}

/// Without the table, the Orb is named after the crate, at API version 1.
#[test]
fn a_manifest_without_the_table_takes_the_defaults() {
    let metadata = read("plain", PACKAGE);
    assert_eq!(metadata.name, "example_greeter");
    assert_eq!(metadata.api_version, 1);
}

/// The table names the Orb and its API version, and a key it leaves out takes its default; its
/// `world`, and a key `cargo tembra build` does not know, are left alone.
#[test]
fn a_manifest_with_the_table_names_the_orb() {
    let metadata = read(
        "named",
        &format!(
            "{PACKAGE}\n[package.metadata.tembra]\nname = \"greeter\"\napi-version = 3\nworld = \"hello\"\n"
        ),
    );
    assert_eq!(metadata.name, "greeter");
    assert_eq!(metadata.api_version, 3);
    let metadata = read(
        "named-only",
        &format!("{PACKAGE}\n[package.metadata.tembra]\nname = \"book\"\nnotes = true\n"),
    );
    assert_eq!(metadata.name, "book");
    assert_eq!(metadata.api_version, 1);
}

/// A table `cargo tembra build` refuses is a compile error naming the manifest.
#[test]
fn an_invalid_table_is_an_error_naming_the_manifest() {
    for (name, table, cause) in [
        (
            "version",
            "[package.metadata.tembra]\napi-version = \"one\"\n",
            "invalid type: string \"one\", expected u32",
        ),
        (
            "negative",
            "[package.metadata.tembra]\napi-version = -1\n",
            "invalid value: integer `-1`, expected u32",
        ),
        (
            "value",
            "[package.metadata]\ntembra = \"greeter\"\n",
            "invalid type: string \"greeter\", expected a table",
        ),
    ] {
        let path = manifest(name, &format!("{PACKAGE}\n{table}"));
        let error = match Metadata::from_manifest(&path) {
            Ok(_) => panic!("the table {table:?} was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.starts_with(&format!(
                "{}: invalid [package.metadata.tembra]: ",
                path.display()
            )) && error.contains(cause),
            "{error}"
        );
    }
}

/// Whether an item is the facade's `__host!`, which wraps the native side.
fn native(item: &Item) -> bool {
    matches!(item, Item::Macro(invocation) if {
        let path = &invocation.mac.path;
        quote::quote!(#path).to_string() == ":: tembra :: __host"
    })
}

/// Only the native side reads the manifest, for the name and API version of `orb()`. A manifest
/// the macro cannot read, one that is no TOML, or none at all, as in a build without Cargo, is a compile
/// error naming it inside the facade's `__host!`, which a component build drops: the component
/// side is the one a manifest that reads gives.
#[test]
fn a_manifest_the_macro_cannot_read_fails_the_native_side_alone() {
    let expanded = |metadata| {
        let options = Options {
            package: Some(literal("example:book@1.0.0")),
            ..Options::default()
        };
        let module = syn::parse_str(
            "mod book { #[service] pub struct Ledger; impl Ledger { pub fn count() -> u32 { 0 } } }",
        )
        .unwrap();
        let tokens =
            expand(options, module, metadata, None).unwrap_or_else(|error| panic!("{error}"));
        let module: ItemMod = syn::parse2(tokens).unwrap();
        module.content.unwrap().1
    };
    let component = |items: &[Item]| {
        let kept = items.iter().filter(|item| !native(item));
        quote::quote!(#(#kept)*).to_string()
    };
    let read = expanded(Ok(Metadata::named("book")));
    let invalid = manifest("invalid", &format!("{PACKAGE}\n[dependencies\n"));
    let missing = std::env::temp_dir()
        .join(format!(
            "tembra-orb-macro-metadata-none-{}",
            std::process::id()
        ))
        .join("Cargo.toml");
    for path in [invalid, missing] {
        let error = match Metadata::from_manifest(&path) {
            Ok(_) => panic!("{} was read", path.display()),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.starts_with(&path.display().to_string()),
            "{message}"
        );
        let items = expanded(Err(error));
        assert_eq!(component(&items), component(&read));
        let wrapped: Vec<&Item> = items.iter().filter(|item| native(item)).collect();
        let [Item::Macro(wrapper)] = wrapped[..] else {
            panic!("{} items wrapped in __host!", wrapped.len());
        };
        let refusal: syn::ItemMacro = wrapper.mac.parse_body().unwrap();
        assert_eq!(
            refusal.mac.parse_body::<syn::LitStr>().unwrap().value(),
            message
        );
    }
}

/// The macro reads the manifest of the crate it expands in: this crate's has no table.
#[test]
fn the_manifest_is_the_crates_own() {
    let metadata = Metadata::read().unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(metadata.name, "tembra-orb-macro");
    assert_eq!(metadata.api_version, 1);
}

/// TOML 1.1 allows a trailing comma in an inline table, which Cargo reads, so the macro reads it.
#[test]
fn a_manifest_with_a_trailing_comma_in_an_inline_table_reads() {
    let text = format!(
        "{PACKAGE}\n[dependencies]\ntembra = {{ version = \"0.2\", features = [\"macros\"], }}\n"
    );
    assert_eq!(read("trailing-comma", &text).name, "example_greeter");
}
