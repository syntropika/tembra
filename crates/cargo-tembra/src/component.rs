//! Reading a component: its WIT, its core imports, and the grants its imports imply.
use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use wasmparser::{Parser, Payload};
use wit_component::{DecodedWasm, WitPrinter};
use wit_parser::{PackageId, Resolve, WorldId, WorldItem, WorldKey};

/// `cargo tembra wit`: print the reconstructed WIT, or write the author's package and a world.
pub fn wit(component: &Path, emit: Option<PathBuf>, world_name: Option<String>) -> Result<()> {
    let bytes = std::fs::read(component)?;
    let (resolve, world) = decode(&bytes)?;
    match emit {
        None => {
            let root = resolve.worlds[world]
                .package
                .context("the world has no package")?;
            let nested: Vec<_> = resolve
                .packages
                .iter()
                .map(|(id, _)| id)
                .filter(|id| *id != root)
                .collect();
            let mut printer = WitPrinter::default();
            printer
                .print(&resolve, root, &nested)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            print!("{}", String::from(printer.output));
        }
        Some(directory) => {
            let name = world_name.context("--emit needs --world <name>")?;
            let text = emit_wit(&resolve, world, &name)?;
            std::fs::create_dir_all(&directory)?;
            let file = directory.join(format!("{name}.wit"));
            std::fs::write(&file, &text)?;
            println!("written {}", file.display());
            print!("{text}");
        }
    }
    Ok(())
}

/// The author's package as WIT text, plus a world with the given name listing what the component
/// imports and exports.
pub fn emit_wit(resolve: &Resolve, world: WorldId, name: &str) -> Result<String> {
    let root = resolve.worlds[world]
        .package
        .context("the world has no package")?;
    let author: PackageId = resolve
        .packages
        .iter()
        .find(|(id, package)| *id != root && package.name.namespace != "wasi")
        .map(|(id, _)| id)
        .context("no author package in the component")?;
    let mut printer = WitPrinter::default();
    printer
        .print(resolve, author, &[])
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut text = String::from(printer.output);
    text.push_str(&format!("\nworld {name} {{\n"));
    for (keyword, side) in [
        ("import", &resolve.worlds[world].imports),
        ("export", &resolve.worlds[world].exports),
    ] {
        for key in side.keys() {
            text.push_str(&format!(
                "  {keyword} {};\n",
                world_key(resolve, key, Some(author))
            ));
        }
    }
    text.push_str("}\n");
    Ok(text)
}

/// `cargo tembra inspect`: core import sections, the summary and, against a WIT, the verdict.
pub fn inspect(
    component: &Path,
    wit_dir: Option<PathBuf>,
    world_name: Option<String>,
) -> Result<()> {
    let bytes = std::fs::read(component)?;
    println!("component: {} ({} bytes)", component.display(), bytes.len());
    // The FFI level: the import sections of the core modules.
    for payload in Parser::new(0).parse_all(&bytes) {
        let Payload::ModuleSection {
            unchecked_range, ..
        } = payload?
        else {
            continue;
        };
        let module = &bytes[unchecked_range.start as usize..unchecked_range.end as usize];
        let mut imports: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for payload in Parser::new(0).parse_all(module) {
            let Payload::ImportSection(reader) = payload? else {
                continue;
            };
            for import in reader.into_imports() {
                let import = import?;
                imports
                    .entry(import.module.to_owned())
                    .or_default()
                    .push(import.name.to_owned());
            }
        }
        if imports.is_empty() {
            continue;
        }
        println!("core module of {} bytes, import section:", module.len());
        for (module, names) in &imports {
            println!("  {module}: {}", names.join(", "));
        }
    }
    let (resolve, world) = decode(&bytes)?;
    summary(&resolve, world, "orb");
    if let Some(wit_dir) = wit_dir {
        let name = world_name.context("--wit needs --world <name>")?;
        let contract = tembra_contracts::Contract::from_dir(&wit_dir, Some(&name))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        println!("checked against world {name} of {}:", wit_dir.display());
        match contract.component_artifact(&bytes, "orb", 1, &Default::default()) {
            Ok(artifact) => println!("  ACCEPTED, artifact of {} bytes", artifact.len()),
            Err(error) => println!("  REFUSED: {error}"),
        }
    }
    Ok(())
}

/// The WIT resolve embedded in a component, and its world.
pub fn decode(bytes: &[u8]) -> Result<(Resolve, WorldId)> {
    match wit_component::decode(bytes).map_err(|error| anyhow::anyhow!("{error}"))? {
        DecodedWasm::Component(resolve, world) => Ok((resolve, world)),
        DecodedWasm::WitPackage(..) => bail!("the file is a WIT package, not a component"),
    }
}

fn world_key(resolve: &Resolve, key: &WorldKey, local: Option<PackageId>) -> String {
    match key {
        WorldKey::Interface(id) if local.is_some() && resolve.interfaces[*id].package == local => {
            resolve.interfaces[*id].name.clone().unwrap_or_default()
        }
        WorldKey::Interface(id) => resolve.id_of(*id).unwrap_or_default(),
        WorldKey::Name(name) => name.clone(),
    }
}

/// Exports as services and methods; imports with the grant they will need at installation.
pub fn summary(resolve: &Resolve, world: WorldId, orb_name: &str) {
    let world = &resolve.worlds[world];
    println!("exports, the services of {orb_name}:");
    for (key, item) in &world.exports {
        let id = world_key(resolve, key, None);
        if let WorldItem::Interface { id: interface, .. } = item {
            let methods: Vec<_> = resolve.interfaces[*interface]
                .functions
                .keys()
                .cloned()
                .collect();
            println!("  {orb_name}::{id}  methods: {}", methods.join(", "));
        } else {
            println!("  {id}");
        }
    }
    let mut requires = Vec::new();
    let mut grants: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (key, item) in &world.imports {
        let WorldItem::Interface { id: interface, .. } = item else {
            continue;
        };
        if resolve.interfaces[*interface].functions.is_empty() {
            continue; // types only: nothing to bind
        }
        let id = world_key(resolve, key, None);
        let kind = match id.split('/').next().unwrap_or_default() {
            "wasi:filesystem" => "filesystem (FilesystemGrant)",
            "wasi:sockets" => "network (NetworkGrant)",
            "wasi:http" => "http (HttpGrant)",
            "wasi:clocks" => "system: clocks",
            "wasi:random" => "system: random",
            "wasi:cli" => "system: cli",
            other if other.starts_with("wasi:") => "NOT SUPPORTED by Tembra",
            _ => {
                requires.push(id);
                continue;
            }
        };
        grants.entry(kind).or_default().push(id);
    }
    if world.imports.is_empty() {
        println!("imports: none; the installer grants nothing");
    }
    if !requires.is_empty() {
        println!("imports of other Orbs, bound with Install::requires:");
        for id in requires {
            println!("  {id}");
        }
    }
    if !grants.is_empty() {
        println!("WASI imports, grants the installer must give:");
        for (kind, ids) in grants {
            println!("  {kind}: {}", ids.join(", "));
        }
    }
}
