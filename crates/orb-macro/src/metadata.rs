//! `[package.metadata.tembra]`, read from the manifest of the crate the macro expands in as
//! `cargo tembra build` reads it: the Orb name and its API version, which the WIT does not carry.
//! The table's `world` is not read: it names the contract of a component without an embedded one,
//! and the macro names its world after the module.
use proc_macro2::Span;
use std::path::{Path, PathBuf};

/// The table's values, each with the default of `cargo tembra build`.
pub(crate) struct Metadata {
    /// `name`, or the crate name.
    pub(crate) name: String,
    /// `api-version`, or 1.
    pub(crate) api_version: u32,
}

/// The table as written, every key optional; a key it does not read is left alone.
#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case", expecting = "a table")]
struct Table {
    name: Option<String>,
    api_version: Option<u32>,
}

impl Metadata {
    /// The table of the crate being compiled, in `CARGO_MANIFEST_DIR/Cargo.toml`.
    pub(crate) fn read() -> syn::Result<Self> {
        let dir = std::env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| error("CARGO_MANIFEST_DIR is not set"))?;
        Self::from_manifest(&PathBuf::from(dir).join("Cargo.toml"))
    }

    /// The table of a manifest. A manifest that does not read, or an invalid table, is an error
    /// naming the manifest.
    pub(crate) fn from_manifest(manifest: &Path) -> syn::Result<Self> {
        // toml ends its messages with a line break.
        let failed = |cause: String| error(format!("{}: {}", manifest.display(), cause.trim_end()));
        let text = std::fs::read_to_string(manifest).map_err(|cause| failed(cause.to_string()))?;
        let document: toml::Value =
            toml::from_str(&text).map_err(|cause| failed(cause.to_string()))?;
        let package = document.get("package");
        let crate_name = package
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| failed("no package.name".to_owned()))?;
        let table: Table = package
            .and_then(|package| package.get("metadata"))
            .and_then(|metadata| metadata.get("tembra"))
            .cloned()
            .map(toml::Value::try_into)
            .transpose()
            .map_err(|cause| failed(format!("invalid [package.metadata.tembra]: {cause}")))?
            .unwrap_or_default();
        Ok(Self {
            name: table.name.unwrap_or_else(|| crate_name.to_owned()),
            api_version: table.api_version.unwrap_or(1),
        })
    }
}

#[cfg(test)]
impl Metadata {
    /// The table of a crate whose Orb is `name` at API version 1, for the tests of the expansion,
    /// which read no manifest.
    pub(crate) fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            api_version: 1,
        }
    }
}

fn error(message: impl std::fmt::Display) -> syn::Error {
    syn::Error::new(Span::call_site(), message.to_string())
}
