//! `cargo tembra new`: an Orb crate using the public facade, from a tembra checkout or from the
//! repository at the commit this cargo-tembra was built from.
use anyhow::{Context, Result, bail};
use heck::ToSnakeCase;
use std::{
    fmt,
    path::{Path, PathBuf},
};

/// The repository a crate outside a checkout takes the facade from: this package's own.
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// The commit of that repository this cargo-tembra was built from, which the build script reads
/// with git; empty for a build without git or outside a git checkout.
const BUILT_FROM: &str = env!("TEMBRA_COMMIT");

/// Where the new crate takes the `tembra` facade from.
#[derive(Debug, PartialEq)]
enum Facade {
    /// The facade crate of a checkout on this machine, `<root>/crates/tembra`.
    Path(String),
    /// The facade in a git repository, at one commit.
    Git { repository: String, rev: String },
}

impl fmt::Display for Facade {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Facade::Path(path) => formatter.write_str(path),
            Facade::Git { repository, rev } => write!(formatter, "{repository} at {rev}"),
        }
    }
}

/// Write the crate skeleton: a manifest with `[package.metadata.tembra]` and one module.
pub fn new(name: &str, package: &str, tembra: Option<PathBuf>) -> Result<()> {
    let checkout = std::env::current_dir()
        .ok()
        .and_then(|current| checkout_above(&current));
    let commit = Some(BUILT_FROM).filter(|commit| !commit.is_empty());
    let facade = facade(tembra, checkout, commit)?;
    if !package.contains(':') || !package.contains('@') {
        bail!("--package must be ns:name@version, for example example:{name}@1.0.0");
    }
    let directory = PathBuf::from(name);
    if directory.exists() {
        bail!("{} already exists", directory.display());
    }
    std::fs::create_dir_all(directory.join("src"))?;
    std::fs::write(directory.join(".gitignore"), "target/\nartifacts/\n")?;
    std::fs::write(directory.join("Cargo.toml"), manifest(name, &facade))?;
    std::fs::write(directory.join("src").join("lib.rs"), library(name, package))?;
    println!("created {}/ with tembra from {facade}", directory.display());
    println!("  cd {} && cargo tembra build", directory.display());
    Ok(())
}

/// The facade the new crate depends on. By path: the checkout `--tembra` names, else the one
/// above the current directory. Outside a checkout, through git: the repository at the commit
/// this cargo-tembra was built from, so that the macro the crate compiles with writes the
/// contract and manifest this CLI reads. A cargo-tembra built without git or outside a git
/// checkout knows no commit, and asks for a checkout rather than depend on whatever the default
/// branch holds.
fn facade(
    tembra: Option<PathBuf>,
    checkout: Option<PathBuf>,
    commit: Option<&str>,
) -> Result<Facade> {
    match (tembra.or(checkout), commit) {
        (Some(root), _) => {
            let canonical = root
                .canonicalize()
                .with_context(|| format!("cannot read {}", root.display()))?;
            let facade_crate = without_verbatim_prefix(canonical)
                .join("crates")
                .join("tembra");
            let path = facade_crate.to_str().with_context(|| {
                format!(
                    "{} is not valid UTF-8, which a Cargo.toml cannot hold",
                    facade_crate.display()
                )
            })?;
            Ok(Facade::Path(path.to_owned()))
        }
        (None, Some(rev)) => Ok(Facade::Git {
            repository: REPOSITORY.to_owned(),
            rev: rev.to_owned(),
        }),
        (None, None) => bail!(
            "no tembra checkout above the current directory, and this cargo-tembra was built \
             without git or outside a git checkout, so it knows no commit of {REPOSITORY} to \
             depend on; pass --tembra <path>"
        ),
    }
}

/// The tembra checkout `directory` lies in: the nearest directory, going up from it, that holds
/// the facade crate.
fn checkout_above(directory: &Path) -> Option<PathBuf> {
    directory
        .ancestors()
        .find(|ancestor| ancestor.join("crates/tembra/Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

/// The path `canonicalize` returned, without the verbatim prefix it gives a drive path on
/// Windows: `\\?\C:\tembra` becomes `C:\tembra`. Any other path stays as it is, a verbatim UNC
/// path included, which would name another location without its prefix.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    let drive = path
        .to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .filter(|rest| {
            let bytes = rest.as_bytes();
            bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
        })
        .map(PathBuf::from);
    drive.unwrap_or(path)
}

/// The comment above a git dependency on the facade: why it stays at one commit.
const PINNED: &str = "\
# The commit cargo-tembra was built from, so that the macro matches the CLI that packages
# the Orb; move it along with cargo-tembra.
";

/// The crate's manifest. The facade's path, repository and commit are written as TOML strings,
/// escaped as TOML requires: the path of a checkout may hold quotes and backslashes.
fn manifest(name: &str, facade: &Facade) -> String {
    let string = |text: &str| toml::Value::String(text.to_owned());
    let (comment, source) = match facade {
        Facade::Path(path) => ("", format!("path = {}", string(path))),
        Facade::Git { repository, rev } => (
            PINNED,
            format!("git = {}, rev = {}", string(repository), string(rev)),
        ),
    };
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
publish = false

[workspace]

[lib]
crate-type = ["cdylib"]

# Read by `cargo tembra build`: the Orb name and API version, which the WIT does not carry.
[package.metadata.tembra]
name = "{name}"
api-version = 1

[dependencies]
{comment}tembra = {{ {source}, default-features = false, features = ["macros"] }}
# The code generated by the macro names these crates: the first two always, serde for a #[state].
wit-bindgen = {{ version = "=0.63.0", default-features = false, features = ["macros", "realloc"] }}
dlmalloc = {{ version = "0.2.12", features = ["global"] }}
serde = {{ version = "1", default-features = false, features = ["derive", "alloc"] }}
serde_json = {{ version = "1", default-features = false, features = ["alloc"] }}

[package.metadata.cargo-machete]
ignored = ["wit-bindgen", "dlmalloc", "serde", "serde_json"]

[profile.release]
panic = "abort"
opt-level = 3
lto = true
strip = "symbols"
"#
    )
}

fn library(name: &str, package: &str) -> String {
    let module = name.to_snake_case();
    format!(
        r#"//! The module is the contract: `cargo tembra build` derives the WIT from the component.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra::orb(package = "{package}", no_std)]
mod {module} {{
    use alloc::{{format, string::String}};

    /// The service: a unit struct marked #[service], with an impl block of public functions.
    #[service]
    pub struct Api;

    impl Api {{
        /// A first method.
        pub fn hello(name: String) -> String {{
            format!("Hello, {{name}}!")
        }}
    }}
}}
"#
    )
}

#[cfg(test)]
mod tests;
