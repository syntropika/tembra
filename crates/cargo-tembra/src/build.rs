//! `cargo tembra build`: compile, derive the WIT, verify, package, summarize.
use crate::component::{contract_text, contributions, decode, summary};
use anyhow::{Context, Result, bail};
use heck::{ToKebabCase, ToSnakeCase};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

/// `[package.metadata.tembra]`: what the WIT does not carry.
#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Metadata {
    /// The Orb name; default: the crate name.
    name: Option<String>,
    /// Default 1.
    api_version: Option<u32>,
    /// The world name of a component without an embedded contract; default: the Orb name in
    /// kebab-case. A component built with the macro names its world itself.
    world: Option<String>,
    /// Priorities of hook contributions; a contribution not listed here has priority 0.
    contributes: BTreeMap<String, i32>,
}

pub fn build(manifest_path: Option<PathBuf>, release: bool, out: Option<PathBuf>) -> Result<()> {
    let manifest_path = manifest_path.unwrap_or_else(|| PathBuf::from("Cargo.toml"));
    let manifest_dir = manifest_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let manifest: toml::Value = toml::from_str(&std::fs::read_to_string(&manifest_path)?)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let crate_name = manifest["package"]["name"]
        .as_str()
        .context("Cargo.toml has no package.name")?
        .to_owned();
    let metadata: Metadata = manifest
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("tembra"))
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .context("invalid [package.metadata.tembra]")?
        .unwrap_or_default();
    let orb_name = metadata.name.clone().unwrap_or_else(|| crate_name.clone());
    let api_version = metadata.api_version.unwrap_or(1);
    let world = metadata
        .world
        .clone()
        .unwrap_or_else(|| orb_name.to_kebab_case());
    let target_dir = manifest_dir.join("target");
    let profile = if release { "release" } else { "debug" };

    // 1. Compile the component on stable: a core module linked by the component linker.
    println!("compiling {crate_name} for wasm32-unknown-unknown ({profile})");
    let mut cargo = Command::new("cargo");
    cargo
        .arg("rustc")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--target-dir")
        .arg(&target_dir);
    if release {
        cargo.arg("--release");
    }
    cargo.arg("--").arg("-Clinker=wasm-component-ld");
    let status = cargo.status().context("cannot run cargo")?;
    if !status.success() {
        bail!(
            "compilation failed; if the target is missing: rustup target add wasm32-unknown-unknown"
        );
    }
    let component_path = target_dir
        .join("wasm32-unknown-unknown")
        .join(profile)
        .join(format!("{}.wasm", crate_name.to_snake_case()));
    let component = std::fs::read(&component_path)
        .with_context(|| format!("missing {}", component_path.display()))?;
    let (resolve, world_id) = decode(&component)?;

    // 2. The contract is an output: the WIT the macro embedded in the component, or the one
    //    derived from its types, written to contract/<world>.wit. Not wit/: wit-bindgen loads a
    //    directory of that name on its own and finds the package twice.
    let (text, world) = contract_text(&component, &resolve, world_id, &world)?;
    let wit_dir = manifest_dir.join("contract");
    let wit_file = wit_dir.join(format!("{world}.wit"));
    std::fs::create_dir_all(&wit_dir)?;
    std::fs::write(&wit_file, text)?;
    println!("contract -> {}", wit_file.display());
    let contract = tembra_contracts::Contract::from_dir(&wit_dir, Some(&world))
        .map_err(|error| anyhow::anyhow!("{error}"))
        .with_context(|| format!("reading the WIT in {}", wit_dir.display()))?;

    // 3. Verify and package: the same component_artifact the SDK build uses for a provider.
    //    Hook contributions are the interfaces the contract both imports and exports.
    let contributions = contributions(&contract, &metadata.contributes)?;
    let artifact = contract
        .component_artifact(&component, &orb_name, api_version, &contributions)
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("the component does not satisfy its contract")?;
    let artifacts = out.unwrap_or_else(|| manifest_dir.join("artifacts"));
    std::fs::create_dir_all(&artifacts)?;
    std::fs::copy(&component_path, artifacts.join(format!("{orb_name}.wasm")))?;
    std::fs::write(artifacts.join(format!("{orb_name}.orb.wasm")), &artifact)?;
    let mut checksums = String::new();
    for (file, bytes) in [
        (format!("{orb_name}.wasm"), &component),
        (format!("{orb_name}.orb.wasm"), &artifact),
    ] {
        checksums.push_str(&format!("{:x}  {file}\n", Sha256::digest(bytes)));
    }
    std::fs::write(artifacts.join("checksums.txt"), &checksums)?;

    // 4. Summary: what the Orb offers, what it imports, which grants an installer must give.
    println!();
    println!("{}/", artifacts.display());
    println!(
        "  {orb_name}.wasm       {} bytes  the component",
        component.len()
    );
    println!(
        "  {orb_name}.orb.wasm   {} bytes  the Tembra artifact ({orb_name} v{api_version})",
        artifact.len()
    );
    println!("  checksums.txt");
    summary(&resolve, world_id, &orb_name, &contributions);
    Ok(())
}
