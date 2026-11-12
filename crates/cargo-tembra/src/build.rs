//! `cargo tembra build`: compile, derive the WIT, verify, package, summarize.
use crate::component::{contract_text, decode, embedded_manifest, summary};
use anyhow::{Context, Result, bail};
use heck::ToKebabCase;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use wit_parser::UnresolvedPackageGroup;

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
    //    The contract is read on its own, before anything is written: no other file in
    //    contract/ takes part in it. The world takes the module's name, so renaming the module
    //    leaves the former contract behind, a file of the same package, which is removed.
    let contract = tembra_contracts::Contract::from_source(
        &wit_file.display().to_string(),
        &text,
        Some(&world),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
    .with_context(|| format!("reading the contract derived for {}", wit_file.display()))?;
    std::fs::create_dir_all(&wit_dir)?;
    // Removed before the new file is written: where file names ignore case, a former name that
    // differs only in case is the new file.
    for former in former_contracts(&wit_dir, &wit_file, &text)? {
        std::fs::remove_file(&former).with_context(|| format!("removing {}", former.display()))?;
        eprintln!(
            "removed {}, the contract of the world's former name",
            former.display()
        );
    }
    std::fs::write(&wit_file, &text)?;
    println!("contract -> {}", wit_file.display());

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

/// The contracts an earlier build wrote into `dir` under a former name of the world: the `.wit`
/// files other than `current` whose own package is the one `text` declares. A file declaring
/// another package, or one that does not parse, is not this tool's output and stays.
fn former_contracts(dir: &Path, current: &Path, text: &str) -> Result<Vec<PathBuf>> {
    let package = |path: &Path, source: &str| {
        UnresolvedPackageGroup::parse(path, source)
            .ok()
            .map(|group| group.main.name)
    };
    let Some(own) = package(current, text) else {
        return Ok(Vec::new());
    };
    let mut formers = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.file_name() == current.file_name()
            || path.extension().is_none_or(|extension| extension != "wit")
            || !path.is_file()
        {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        if package(&path, &source).as_ref() == Some(&own) {
            formers.push(path);
        }
    }
    formers.sort();
    Ok(formers)
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
