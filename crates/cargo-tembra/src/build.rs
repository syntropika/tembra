//! `cargo tembra build`: compile, derive the WIT, verify, package, summarize.
use crate::component::{contract_text, decode, embedded_manifest, summary};
use anyhow::{Context, Result, bail};
use heck::ToKebabCase;
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
}

/// One line of cargo's JSON output, as far as `build` reads it: a `compiler-artifact` message
/// names the manifest of its package, the kinds of its target and the files it produced.
#[derive(serde::Deserialize)]
struct Message {
    reason: String,
    #[serde(default)]
    manifest_path: PathBuf,
    #[serde(default)]
    target: Target,
    #[serde(default)]
    filenames: Vec<PathBuf>,
}

/// The target of a compiled artifact; an Orb's library has the kind `cdylib`.
#[derive(Default, serde::Deserialize)]
struct Target {
    kind: Vec<String>,
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
    // Not by index: a manifest without [package], a virtual workspace root, would panic.
    let crate_name = manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
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

    // 1. Compile the component on stable: a core module linked by the component linker. Cargo
    //    reports the files it built as JSON on stdout and still renders the diagnostics on
    //    stderr. The component is the file cargo names: a name derived from the crate's misses
    //    a [lib] name, and may find a file an earlier build left.
    println!("compiling {crate_name} for wasm32-unknown-unknown ({profile})");
    let package = manifest_path
        .canonicalize()
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let mut cargo = Command::new("cargo");
    cargo
        .arg("rustc")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--target-dir")
        .arg(&target_dir)
        .arg("--message-format=json-render-diagnostics")
        .stdout(Stdio::piped());
    if release {
        cargo.arg("--release");
    }
    cargo.arg("--").arg("-Clinker=wasm-component-ld");
    let mut child = cargo.spawn().context("cannot run cargo")?;
    let messages = child
        .stdout
        .take()
        .context("cannot read the messages of cargo")?;
    let built = built_component(BufReader::new(messages), &package);
    let status = child.wait().context("cannot run cargo")?;
    let built = built?;
    if !status.success() {
        bail!(
            "compilation failed; if the target is missing: rustup target add wasm32-unknown-unknown"
        );
    }
    let component_path = built.with_context(|| {
        format!(
            "cargo built no cdylib for {crate_name}; an Orb's [lib] has crate-type = [\"cdylib\"]"
        )
    })?;
    let component = std::fs::read(&component_path)
        .with_context(|| format!("missing {}", component_path.display()))?;
    let (resolve, world_id) = decode(&component)?;

    // 2. The contract is an output: the WIT the macro embedded in the component, or the one
    //    derived from its types, written to contract/<world>.wit. Not wit/: wit-bindgen loads a
    //    directory of that name on its own and finds the package twice. The packages the module
    //    consumes are nested whole from the directory the macro named.
    let embedded = embedded_manifest(&component)?;
    let consumed = embedded.wit.as_ref().map(|dir| manifest_dir.join(dir));
    let (text, world) = contract_text(&component, &resolve, world_id, &world, consumed.as_deref())?;
    let wit_dir = manifest_dir.join("contract");
    let wit_file = wit_dir.join(format!("{world}.wit"));
    std::fs::create_dir_all(&wit_dir)?;
    std::fs::write(&wit_file, text)?;
    println!("contract -> {}", wit_file.display());
    let contract = tembra_contracts::Contract::from_dir(&wit_dir, Some(&world))
        .map_err(|error| anyhow::anyhow!("{error}"))
        .with_context(|| format!("reading the WIT in {}", wit_dir.display()))?;

    // 3. Verify and package: the same component_artifact the SDK build uses for a provider.
    //    Hook interfaces are the ones the contract both imports and exports; which are declared,
    //    with what composition, and which are contributions, with what priority, travels in the
    //    component with the lifetime each service chose: the macro writes them from the #[hook]
    //    and #[service] attributes.
    let roles = crate::component::roles(&contract, &embedded)?;
    let artifact = contract
        .component_artifact(&component, &orb_name, api_version, &roles)
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("the component does not satisfy its contract")?;
    //    The outputs stay inside the crate, next to contract/; with --out they are collected
    //    elsewhere together with a copy of the contract, so one directory can hold every Orb.
    let collected = out.is_some();
    let artifacts = out.unwrap_or_else(|| manifest_dir.join("artifacts"));
    std::fs::create_dir_all(&artifacts)?;
    std::fs::copy(&component_path, artifacts.join(format!("{orb_name}.wasm")))?;
    std::fs::write(artifacts.join(format!("{orb_name}.orb.wasm")), &artifact)?;
    let mut files = vec![
        (format!("{orb_name}.wasm"), component.clone()),
        (format!("{orb_name}.orb.wasm"), artifact.clone()),
    ];
    if collected {
        let name = format!("{world}.wit");
        std::fs::copy(&wit_file, artifacts.join(&name))?;
        files.push((name, std::fs::read(&wit_file)?));
    }
    let mut checksums = String::new();
    for (file, bytes) in &files {
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
    if collected {
        println!("  {world}.wit         the contract, copied from contract/");
    }
    println!("  checksums.txt");
    summary(&resolve, world_id, &orb_name, &roles);
    Ok(())
}

/// The component cargo built for the package whose canonical manifest is `package`: the `.wasm`
/// file of the package's `cdylib` target, as cargo's `compiler-artifact` message names it, for a
/// fresh target too. Lines that are not cargo messages, a procedural macro's prints, go to stdout
/// as they would without the JSON format.
fn built_component(messages: impl BufRead, package: &Path) -> Result<Option<PathBuf>> {
    let mut component = None;
    for line in messages.lines() {
        let line = line.context("reading the messages of cargo")?;
        let Ok(message) = serde_json::from_str::<Message>(&line) else {
            println!("{line}");
            continue;
        };
        if message.reason == "compiler-artifact"
            && message.target.kind.iter().any(|kind| kind == "cdylib")
            && message
                .manifest_path
                .canonicalize()
                .is_ok_and(|manifest| manifest == package)
        {
            component = message.filenames.into_iter().find(|file| {
                file.extension()
                    .is_some_and(|extension| extension == "wasm")
            });
        }
    }
    Ok(component)
}

#[cfg(test)]
mod tests;
