//! Reading a component: its contract, its core imports, and the grants its imports imply.
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use tembra_contracts::{Contract, HOST, LIFECYCLE};
use wasmparser::{Parser, Payload};
use wit_component::{DecodedWasm, WitPrinter};
use wit_parser::{InterfaceId, PackageId, Resolve, WorldId, WorldItem, WorldKey};

/// The custom section in which the macro stores the WIT it derived from the module.
const WIT_SECTION: &str = "tembra:wit";
/// The custom section in which the macro stores the directory of the WIT packages the module
/// consumes, relative to the crate manifest, as written on the attribute.
const CONSUMED_SECTION: &str = "tembra:consumed";

/// `cargo tembra wit`: print the reconstructed WIT, or write the contract with --emit.
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
            let (text, name) = contract_text(&bytes, &resolve, world, &name, None)?;
            std::fs::create_dir_all(&directory)?;
            let file = directory.join(format!("{name}.wit"));
            std::fs::write(&file, &text)?;
            println!("written {}", file.display());
            print!("{text}");
        }
    }
    Ok(())
}

/// The contract of a component as WIT text, and the name of its world.
///
/// A component built with the macro carries the module's own WIT in a custom section; the
/// packages it consumes are nested after it, whole, from the directory the macro named when it
/// is given, else from the component's types, which keep only the functions the component calls.
/// Any other component gets the WIT derived from its types, with the given world name.
pub fn contract_text(
    bytes: &[u8],
    resolve: &Resolve,
    world: WorldId,
    name: &str,
    consumed: Option<&Path>,
) -> Result<(String, String)> {
    let root = resolve.worlds[world]
        .package
        .context("the world has no package")?;
    match embedded_wit(bytes)? {
        Some(text) => {
            let package = text
                .lines()
                .find_map(|line| line.strip_prefix("package ")?.strip_suffix(';'))
                .context("the tembra:wit section declares no package")?
                .trim()
                .to_owned();
            let world_name = text
                .lines()
                .find_map(|line| line.trim_start().strip_prefix("world "))
                .map(|rest| rest.trim_end_matches('{').trim().to_owned())
                .context("the tembra:wit section declares no world")?;
            // The whole consumed packages, when the macro said where they are: a provider's
            // interface keeps the functions this component never calls, so the contract names
            // the same interface the provider exports.
            let whole = match consumed {
                Some(dir) => {
                    let mut whole = Resolve::default();
                    whole.push_dir(dir).with_context(|| {
                        format!("reading the consumed WIT in {}", dir.display())
                    })?;
                    Some(whole)
                }
                None => None,
            };
            let mut text = text;
            for id in nested(resolve, root, |name| *name != package) {
                let (source, id) = match &whole {
                    Some(whole) => match whole.package_names.get(&resolve.packages[id].name) {
                        Some(id) => (whole, *id),
                        None => (resolve, id),
                    },
                    None => (resolve, id),
                };
                let mut printer = WitPrinter::default();
                printer
                    .print_package(source, id, false)
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                text.push('\n');
                text.push_str(&String::from(printer.output));
            }
            Ok((text, world_name))
        }
        None => Ok((derived_wit(resolve, world, name)?, name.to_owned())),
    }
}

/// The `tembra:wit` custom section of the component's core module, if the macro wrote one.
fn embedded_wit(bytes: &[u8]) -> Result<Option<String>> {
    embedded_section(bytes, WIT_SECTION)?
        .map(|data| String::from_utf8(data).context("the tembra:wit section is not UTF-8"))
        .transpose()
}

/// The directory of the consumed WIT packages, relative to the crate manifest, if the module
/// names one.
pub fn embedded_consumed(bytes: &[u8]) -> Result<Option<String>> {
    embedded_section(bytes, CONSUMED_SECTION)?
        .map(|data| String::from_utf8(data).context("the tembra:consumed section is not UTF-8"))
        .transpose()
}

/// A custom section of the component's core module, by name.
fn embedded_section(bytes: &[u8], name: &str) -> Result<Option<Vec<u8>>> {
    for payload in Parser::new(0).parse_all(bytes) {
        let Payload::ModuleSection {
            unchecked_range, ..
        } = payload?
        else {
            continue;
        };
        let module = &bytes[unchecked_range.start as usize..unchecked_range.end as usize];
        for payload in Parser::new(0).parse_all(module) {
            let Payload::CustomSection(section) = payload? else {
                continue;
            };
            if section.name() == name {
                return Ok(Some(section.data().to_vec()));
            }
        }
    }
    Ok(None)
}

/// The author's package as WIT text, the consumed packages nested in it, and a world with the
/// given name listing what the component imports and exports.
fn derived_wit(resolve: &Resolve, world: WorldId, name: &str) -> Result<String> {
    let root = resolve.worlds[world]
        .package
        .context("the world has no package")?;
    let author = author_package(resolve, world)?;
    let consumed: Vec<PackageId> = nested(resolve, root, |_| true)
        .filter(|id| *id != author)
        .collect();
    let mut printer = WitPrinter::default();
    printer
        .print(resolve, author, &consumed)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut text = String::from(printer.output);
    text.push_str(&format!("\nworld {name} {{\n"));
    for (keyword, side) in [
        ("import", &resolve.worlds[world].imports),
        ("export", &resolve.worlds[world].exports),
    ] {
        for key in side.keys() {
            let name = world_key(resolve, key, Some(author));
            // The kernel's private package never enters a contract.
            if name.starts_with("tembra:") {
                continue;
            }
            text.push_str(&format!("  {keyword} {name};\n"));
        }
    }
    text.push_str("}\n");
    Ok(text)
}

/// The packages a contract must carry along: not the synthesized root, not the packages that ship
/// with tembra-contracts (WASI, and the kernel's private `tembra:orb`, which never enters a
/// contract), and only those accepted by the filter.
fn nested<'a>(
    resolve: &'a Resolve,
    root: PackageId,
    keep: impl Fn(&String) -> bool + 'a,
) -> impl Iterator<Item = PackageId> + 'a {
    resolve
        .packages
        .iter()
        .filter(move |(id, package)| {
            *id != root
                && !matches!(package.name.namespace.as_str(), "wasi" | "tembra")
                && keep(&package.name.to_string())
        })
        .map(|(id, _)| id)
}

/// The package the component defines: the one owning the interfaces it exports without importing.
fn author_package(resolve: &Resolve, world: WorldId) -> Result<PackageId> {
    let world = &resolve.worlds[world];
    let imported: BTreeSet<InterfaceId> = world.imports.values().filter_map(interface).collect();
    world
        .exports
        .values()
        .filter_map(interface)
        .filter(|id| !imported.contains(id))
        .find_map(|id| resolve.interfaces[id].package)
        .context(
            "the component exports no interface of its own; an Orb declares at least one service",
        )
}

fn interface(item: &WorldItem) -> Option<InterfaceId> {
    match item {
        WorldItem::Interface { id, .. } => Some(*id),
        _ => None,
    }
}

/// The interfaces a contract imports, by identity. A hook interface the component imports and the
/// contract imports too is a contribution; one the contract does not import is a hook the
/// component declares, which it imports only to run it.
pub fn contract_imports(contract: &Contract) -> BTreeSet<String> {
    contract
        .prepared()
        .model()
        .imports
        .iter()
        .filter_map(|entry| match &entry.item {
            tembra_contracts::Item::Interface(interface) => interface.contract.clone(),
            _ => None,
        })
        .collect()
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
    let contract = match wit_dir {
        Some(wit_dir) => {
            let name = world_name.context("--wit needs --world <name>")?;
            let contract = Contract::from_dir(&wit_dir, Some(&name))
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            Some((contract, name, wit_dir))
        }
        None => None,
    };
    let imports = contract
        .as_ref()
        .map(|(contract, ..)| contract_imports(contract));
    summary(&resolve, world, "orb", imports.as_ref());
    if let Some((contract, name, wit_dir)) = contract {
        println!("checked against world {name} of {}:", wit_dir.display());
        match contract.source_component_artifact(&bytes, "orb", 1) {
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

/// Exports as services and methods, hooks declared and contributed to, and imports with the grant
/// they will need at installation.
///
/// The component's world is the one its bindings were built from: the kernel's private
/// `tembra:orb` package, which the host links itself, is left out. An exported interface the
/// contract also imports is a contribution; one only the component imports, to run it, is a hook
/// the component declares. Without the contract, a hook interface the component imports is
/// listed as such and a contribution it never calls looks like a service. The lifetime and linger
/// of each service travel in `lifecycle.describe`.
pub fn summary(
    resolve: &Resolve,
    world: WorldId,
    orb_name: &str,
    contract_imports: Option<&BTreeSet<String>>,
) {
    let world = &resolve.worlds[world];
    let private = |id: &str| id.starts_with("tembra:orb/");
    let imported: BTreeSet<String> = world
        .imports
        .keys()
        .map(|key| world_key(resolve, key, None))
        .collect();
    let (mut declared, mut contributed, mut hooks) = (Vec::new(), Vec::new(), Vec::new());
    let mut lifecycle = false;
    println!("exports, the services of {orb_name}:");
    for (key, item) in &world.exports {
        let id = world_key(resolve, key, None);
        if private(&id) {
            lifecycle = true;
            continue;
        }
        // A contribution's import is in the contract, but the linker drops it from a component
        // that never calls it: the contract decides. A declared hook is imported by the
        // component alone, to run it.
        match contract_imports {
            Some(imports) if imports.contains(&id) => {
                contributed.push(id);
                continue;
            }
            Some(_) if imported.contains(&id) => {
                declared.push(id);
                continue;
            }
            None if imported.contains(&id) => {
                hooks.push(id);
                continue;
            }
            _ => {}
        }
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
    if !declared.is_empty() {
        println!("hooks declared, run by the kernel's plan of contributions:");
        for id in &declared {
            println!("  {id}");
        }
    }
    if !contributed.is_empty() {
        println!("hook contributions, bound to the provider with Install::requires:");
        for id in &contributed {
            println!("  {id}");
        }
    }
    if !hooks.is_empty() {
        println!("hook interfaces, declared or contributed to (--wit tells them apart):");
        for id in &hooks {
            println!("  {id}");
        }
    }
    if lifecycle {
        println!(
            "kernel lifecycle: the host describes, creates and releases this Orb through {LIFECYCLE} and answers its state, resources, hooks and context through {HOST}"
        );
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
        if private(&id)
            || declared.contains(&id)
            || contributed.contains(&id)
            || hooks.contains(&id)
        {
            // Served by the kernel, bound to the provider through the contribution, or the entry
            // to the Orb's own hook.
            continue;
        }
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
    if requires.is_empty() && grants.is_empty() {
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
