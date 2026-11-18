//! `cargo tembra build`: compile, derive the WIT, verify, package, summarize.
use crate::component::{decode, derived_wit, summary};
use anyhow::{Context, Result, bail};
use heck::ToKebabCase;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tembra_contracts::{EmbeddedManifest, ModuleContract};

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
    let target_dir = target_directory(&manifest_path)?;
    let profile = if release { "release" } else { "debug" };
    // What contract/ holds before the compilation, in which the macro writes it too.
    let contract_dir = manifest_dir.join("contract");
    let before = contract_files(&contract_dir)?;

    // 1. Compile the component on stable: a core module linked by the component linker. The
    //    library is compiled as a cdylib whatever crate type the manifest names, so that an Orb
    //    crate keeps the default one and a native host can depend on it. Cargo reports the files
    //    it built as JSON on stdout and still renders the diagnostics on stderr. The component is
    //    the file cargo names: a name derived from the crate's misses a [lib] name, and may find
    //    a file an earlier build left.
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
        .arg("--crate-type")
        .arg("cdylib")
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
    let component_path =
        built.with_context(|| format!("cargo built no cdylib for {crate_name}"))?;
    let component = std::fs::read(&component_path)
        .with_context(|| format!("missing {}", component_path.display()))?;
    let (resolve, world_id) = decode(&component)?;

    // 2. The contract is an output: the WIT the macro embedded in the component, with the
    //    packages the module consumes nested whole from the directory the macro named, or the one
    //    derived from the component's types, written to contract/<world>.wit with the standard
    //    packages it depends on in contract/deps/. The macro writes the same files when the crate
    //    is compiled for itself; `ModuleContract` is the one writer of both. The world takes the
    //    module's name, so renaming the module leaves the former contract behind, a file of the
    //    same package, which the write removes. The macro's write, during the compilation, removes
    //    it first, so the build names what went from the files contract/ held before.
    let consumed = EmbeddedManifest::read(&component)
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .wit
        .map(|dir| manifest_dir.join(dir));
    let module = match ModuleContract::from_component(&component, consumed.as_deref())
        .map_err(|error| anyhow::anyhow!("{error}"))?
    {
        Some(module) => module,
        None => ModuleContract::new(&derived_wit(&resolve, world_id, &world)?, None)
            .map_err(|error| anyhow::anyhow!("{error}"))
            .context("reading the contract derived from the component")?,
    };
    let world = module.world().to_owned();
    let written = module
        .write(&manifest_dir)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    for notice in removed(&contract_dir, &before)? {
        eprintln!("{notice}");
    }
    let wit_file = written.contract;
    println!("contract -> {}", wit_file.display());
    for file in &written.dependencies {
        println!("contract -> {}", file.display());
    }
    let contract = module.contract();

    // 3. Verify and package: the same component_artifact the SDK build uses for a provider.
    //    Hook interfaces are the ones the contract both imports and exports; which are declared,
    //    with what composition, and which are contributions, with what priority, travels in the
    //    component with the lifetime each service chose: the macro writes them from the #[hook]
    //    and #[service] attributes.
    let roles = crate::component::roles(contract, &component)?;
    let artifact = contract
        .component_artifact(&component, &orb_name, api_version, &roles)
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("the component does not satisfy its contract")?;
    //    The outputs stay inside the crate, next to contract/; with --out they are collected
    //    elsewhere together with a copy of the contract, so one directory can hold every Orb:
    //    checksums.txt keeps the lines of the files this build does not write.
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
    let checksums = artifacts.join("checksums.txt");
    let previous = match std::fs::read_to_string(&checksums) {
        Ok(previous) => previous,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", checksums.display()));
        }
    };
    std::fs::write(&checksums, merged_checksums(&previous, &files))?;

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
    summary(
        &mut std::io::stdout(),
        &resolve,
        world_id,
        &orb_name,
        &roles,
        Some(contract.prepared().model()),
    )?;
    Ok(())
}

/// The directory Cargo builds the package of `manifest_path` into: `target_directory` of
/// `cargo metadata`, which follows `CARGO_TARGET_DIR`, `build.target-dir` and the workspace the
/// package belongs to.
fn target_directory(manifest_path: &Path) -> Result<PathBuf> {
    #[derive(serde::Deserialize)]
    struct Metadata {
        target_directory: PathBuf,
    }
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version=1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(manifest_path)
        .stderr(Stdio::inherit())
        .output()
        .context("cannot run cargo")?;
    if !output.status.success() {
        bail!("cargo metadata failed for {}", manifest_path.display());
    }
    let metadata: Metadata = serde_json::from_slice(&output.stdout)
        .context("reading the target directory from cargo metadata")?;
    Ok(metadata.target_directory)
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

/// The `.wit` files of the contract directory `dir`, then those of its `deps/`, each sorted: the
/// files a write of the contract removes, the contract of a world's former name and the standard
/// packages no world depends on any more. Empty when the directory does not exist yet.
fn contract_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for dir in [dir.to_path_buf(), dir.join("deps")] {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", dir.display()));
            }
        };
        let mut found = Vec::new();
        for entry in entries {
            let path = entry
                .with_context(|| format!("reading {}", dir.display()))?
                .path();
            if path.extension().is_some_and(|extension| extension == "wit") && path.is_file() {
                found.push(path);
            }
        }
        found.sort();
        files.extend(found);
    }
    Ok(files)
}

/// What the build says on stderr of each file of the contract directory `dir` removed since it
/// held the files `before`, in their order: the macro writes the contract while cargo compiles
/// the crate, so the build's own write finds removed already what the compilation removed.
fn removed(dir: &Path, before: &[PathBuf]) -> Result<Vec<String>> {
    let after = contract_files(dir)?;
    let deps = dir.join("deps");
    Ok(before
        .iter()
        .filter(|file| !after.contains(file))
        .map(|file| {
            if file.parent() == Some(deps.as_path()) {
                format!(
                    "removed {}, a standard package the contract no longer depends on",
                    file.display()
                )
            } else {
                format!(
                    "removed {}, the contract of the world's former name",
                    file.display()
                )
            }
        })
        .collect())
}

/// The checksums of a directory after a build, `previous` being the file's former content: one
/// `sha256  file` line per file, sorted by file name. The files this build wrote get new lines;
/// the lines of the others, another Orb's in a directory given to `--out`, stay as they were.
fn merged_checksums(previous: &str, written: &[(String, Vec<u8>)]) -> String {
    let mut lines: BTreeMap<&str, String> = previous
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let file = line.split_once("  ").map_or(line, |(_, file)| file);
            (file, line.to_owned())
        })
        .collect();
    for (file, bytes) in written {
        lines.insert(file, format!("{:x}  {file}", Sha256::digest(bytes)));
    }
    lines.into_values().map(|line| line + "\n").collect()
}

#[cfg(test)]
mod tests;
