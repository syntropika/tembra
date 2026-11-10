//! Reading a component: its contract, its core imports, and the grants its imports imply.
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use tembra_contracts::{CallbackRoles, Contract, HookComposition, guest};
use wasmparser::{Parser, Payload};
use wit_component::{DecodedWasm, WitPrinter};
use wit_parser::{InterfaceId, PackageId, Resolve, WorldId, WorldItem, WorldKey};

/// The custom section in which the macro stores the WIT it derived from the module.
const WIT_SECTION: &str = "tembra:wit";
/// The custom section in which the macro stores the hook roles, as a JSON object.
const HOOKS_SECTION: &str = "tembra:hooks";

/// The hook roles the macro stored in the component: the hooks it contributes to, with their
/// priorities, and the hooks it declares, with their composition.
#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddedHooks {
    pub contributes: BTreeMap<String, i32>,
    pub declares: BTreeMap<String, HookComposition>,
}

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
            let (text, name) = contract_text(&bytes, &resolve, world, &name)?;
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
/// packages it consumes are nested after it, taken from the component's types. Any other
/// component gets the WIT derived from its types, with the given world name.
pub fn contract_text(
    bytes: &[u8],
    resolve: &Resolve,
    world: WorldId,
    name: &str,
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
            let mut text = text;
            for id in nested(resolve, root, |name| *name != package) {
                let mut printer = WitPrinter::default();
                printer
                    .print_package(resolve, id, false)
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

/// The hook roles the macro stored in the component:
/// `{"contributes": {"ns:name/hook@version": n}, "declares": {"ns:name/hook@version": "waterfall"}}`.
pub fn embedded_hooks(bytes: &[u8]) -> Result<EmbeddedHooks> {
    match embedded_section(bytes, HOOKS_SECTION)? {
        Some(data) => serde_json::from_slice(&data)
            .context("the tembra:hooks section is not a JSON object of hook roles"),
        None => Ok(EmbeddedHooks::default()),
    }
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
            text.push_str(&format!(
                "  {keyword} {};\n",
                world_key(resolve, key, Some(author))
            ));
        }
    }
    text.push_str("}\n");
    Ok(text)
}

/// The packages a contract must carry along: not the synthesized root, not the standard packages
/// that ship with tembra-contracts (WASI and the kernel's `tembra:guest`), and only those
/// accepted by the filter.
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

/// The hook roles of a contract. A hook interface is one the world both imports and exports: the
/// ones the component declares, with their composition, are its own hooks; the others are
/// contributions, with the priority the component carries, or 0. A role the component names
/// for an interface that is not such a pair is an error.
pub fn roles(contract: &Contract, embedded: &EmbeddedHooks) -> Result<CallbackRoles> {
    let model = contract.prepared().model();
    let identities = |entries: &[tembra_contracts::Entry]| -> BTreeSet<String> {
        entries
            .iter()
            .filter_map(|entry| match &entry.item {
                tembra_contracts::Item::Interface(interface) => interface.contract.clone(),
                _ => None,
            })
            .collect()
    };
    let exported = identities(&model.exports);
    let paired: BTreeSet<String> = identities(&model.imports)
        .into_iter()
        .filter(|id| exported.contains(id))
        .collect();
    for id in embedded.declares.keys() {
        if !paired.contains(id) {
            bail!("#[hook] declares {id}, but the contract does not import and export it");
        }
    }
    let mut contributions: BTreeMap<String, i32> = paired
        .iter()
        .filter(|id| !embedded.declares.contains_key(*id))
        .map(|id| (id.clone(), 0))
        .collect();
    for (id, priority) in &embedded.contributes {
        let slot = contributions.get_mut(id).with_context(|| {
            format!("#[hook] contributes to {id}, but the contract does not import and export it as a contribution")
        })?;
        *slot = *priority;
    }
    Ok(CallbackRoles {
        contributions,
        hooks: embedded.declares.clone(),
    })
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
    let roles = match &contract {
        Some((contract, ..)) => roles(contract, &embedded_hooks(&bytes)?)?,
        None => CallbackRoles::default(),
    };
    summary(&resolve, world, "orb", &roles);
    if let Some((contract, name, wit_dir)) = contract {
        println!("checked against world {name} of {}:", wit_dir.display());
        match contract.component_artifact(&bytes, "orb", 1, &roles) {
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

/// Exports as services and methods, hooks declared and contributed to, kernel-managed state and
/// resources, and imports with the grant they will need at installation.
pub fn summary(resolve: &Resolve, world: WorldId, orb_name: &str, roles: &CallbackRoles) {
    let CallbackRoles {
        contributions,
        hooks,
    } = roles;
    let world = &resolve.worlds[world];
    let (mut state, mut resources) = (false, false);
    println!("exports, the services of {orb_name}:");
    for (key, item) in &world.exports {
        let id = world_key(resolve, key, None);
        if contributions.contains_key(&id) || hooks.contains_key(&id) {
            continue; // a hook callback, not a service
        }
        if guest::reserved(&id) {
            // The kernel's callbacks, answered to the kernel.
            state |= id == guest::STATE_INIT;
            resources |= id == guest::RESOURCE_INIT;
            continue;
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
    if !hooks.is_empty() {
        println!("hooks declared, run by the kernel's plan of contributions:");
        for (id, composition) in hooks {
            println!(
                "  {id}  {}",
                serde_json::json!(composition).as_str().unwrap_or_default()
            );
        }
    }
    if !contributions.is_empty() {
        println!("hook contributions, bound to the provider with Install::requires:");
        for (id, priority) in contributions {
            println!("  {id}  priority {priority}");
        }
    }
    if state {
        println!(
            "kernel-managed state: the kernel keeps the state of this Orb's service objects ({})",
            guest::STATE
        );
    }
    if resources {
        println!(
            "kernel-managed resources: the kernel opens and closes the resources of this Orb's root ({})",
            guest::RESOURCE
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
        if contributions.contains_key(&id) || hooks.contains_key(&id) || guest::reserved(&id) {
            // Bound to the provider through the contribution, the entry to the Orb's own hook,
            // or served by the kernel.
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
        if contributions.is_empty() && hooks.is_empty() {
            println!("imports: none; the installer grants nothing");
        } else {
            println!("other imports: none; the installer grants nothing");
        }
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
