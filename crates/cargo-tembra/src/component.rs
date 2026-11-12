//! Reading a component: its contract, its core imports, and the grants its imports imply.
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use tembra_contracts::{
    ComponentRoles, Contract, HTTP_HANDLER, Lifetime, ServicePolicy, SystemCapability, guest,
    system_capability,
};
use wasmparser::{Parser, Payload};
use wit_component::{DecodedWasm, WitPrinter};
use wit_parser::{
    FunctionKind, InterfaceId, PackageId, PackageName, Resolve, UnresolvedPackageGroup, WorldId,
    WorldItem, WorldKey,
};

/// The custom section in which the macro stores the WIT it derived from the module.
const WIT_SECTION: &str = "tembra:wit";

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
/// A package a nested one builds on is nested too, though the component's types do not reach it.
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
            let consumed = consumed_packages(resolve, root, &package, &text, whole.as_ref());
            let mut text = text;
            for (source, id) in consumed {
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
            *id != root && !standard(&package.name) && keep(&package.name.to_string())
        })
        .map(|(id, _)| id)
}

/// Whether a package is in a namespace of the standard packages that ship with tembra-contracts,
/// which a contract never nests.
pub(crate) fn standard(name: &PackageName) -> bool {
    matches!(name.namespace.as_str(), "wasi" | "tembra")
}

/// The packages the module's own package `own`, written as `text`, consumes, each with the
/// resolve to print it from: the consumed directory `whole` when it holds the package, else the
/// component's types. First those of the component's types, in their order, then any other the
/// module's WIT names, which the linker may have dropped from the types with an import the code
/// never calls, then any other these packages build on: a package printed whole may name one the
/// component never reaches. A package found nowhere is left to the reading of the contract, which
/// names it.
fn consumed_packages<'a>(
    resolve: &'a Resolve,
    root: PackageId,
    own: &str,
    text: &str,
    whole: Option<&'a Resolve>,
) -> Vec<(&'a Resolve, PackageId)> {
    // Each package wanted, by name, with the resolve that names it.
    let mut wanted: Vec<(PackageName, &'a Resolve)> = nested(resolve, root, |name| name != own)
        .map(|id| (resolve.packages[id].name.clone(), resolve))
        .collect();
    if let Ok(group) = UnresolvedPackageGroup::parse("contract.wit", text) {
        wanted.extend(
            group
                .main
                .foreign_deps
                .into_keys()
                .map(|name| (name, resolve)),
        );
    }
    let mut packages: Vec<(&'a Resolve, PackageId)> = Vec::new();
    let mut next = 0;
    while let Some((name, named_by)) = wanted.get(next).cloned() {
        next += 1;
        let taken = packages
            .iter()
            .any(|(source, id)| source.packages[*id].name == name);
        if standard(&name) || name.to_string() == own || taken {
            continue;
        }
        let Some((source, id)) = whole
            .into_iter()
            .chain([named_by])
            .find_map(|source| Some((source, *source.package_names.get(&name)?)))
        else {
            continue;
        };
        packages.push((source, id));
        wanted.extend(
            source
                .package_direct_deps(id)
                .map(|dependency| (source.packages[dependency].name.clone(), source)),
        );
    }
    packages
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

/// The component roles of a contract. A hook interface is one the world both imports and
/// exports: the component's manifest says which it declares, with their composition, which it
/// contributes to, with their priorities, which of its services chose a lifetime or a linger, and
/// which imports are the views of interfaces it imports by key, as `Contract::embedded_roles`
/// reads and checks them. Every other hook interface is a contribution at priority 0, as for a
/// component built without the macro, which carries no manifest.
pub fn roles(contract: &Contract, component: &[u8]) -> Result<ComponentRoles> {
    let mut roles = contract
        .embedded_roles(component)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
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
    for id in identities(&model.imports) {
        if exported.contains(&id) && !roles.hooks.contains_key(&id) {
            roles.contributions.entry(id).or_insert(0);
        }
    }
    Ok(roles)
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
        Some((contract, ..)) => self::roles(contract, &bytes)?,
        None => ComponentRoles::default(),
    };
    summary(&resolve, world, "orb", &roles);
    if let Some((contract, name, wit_dir)) = contract {
        println!("checked against world {name} of {}:", wit_dir.display());
        match contract.component_artifact(&bytes, "orb", 1, &roles) {
            Ok(artifact) => println!("  ACCEPTED, artifact of {} bytes", artifact.len()),
            Err(error) => {
                println!("  REFUSED: {error}");
                bail!(
                    "the component does not satisfy world {name} of {}",
                    wit_dir.display()
                );
            }
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

/// Whether an export is the incoming HTTP handler, by the rule of `tembra_contracts`: the exact
/// `wasi:http/handler@0.3.0` interface under its own name, with the standard `handle` function.
/// The host's ingress calls it; it is never a service.
fn is_ingress(resolve: &Resolve, key: &WorldKey) -> bool {
    let WorldKey::Interface(id) = key else {
        return false;
    };
    let functions: Vec<_> = resolve.interfaces[*id].functions.values().collect();
    resolve.id_of(*id).as_deref() == Some(HTTP_HANDLER)
        && matches!(
            functions.as_slice(),
            [function] if function.name == "handle"
                && function.kind == FunctionKind::AsyncFreestanding
                && function.params.len() == 1
                && function.result.is_some()
        )
}

/// What installation asks for an import of a component.
#[derive(Debug, PartialEq, Eq)]
enum Import {
    /// An interface of another Orb, bound with `Install::requires`.
    Orb,
    /// A standard WASI interface: the grant it needs, or that Tembra does not support it.
    Wasi(&'static str),
    /// The HTTP types of an Orb that serves incoming HTTP without importing the HTTP client:
    /// bound to the host's ingress, with no grant.
    Ingress,
}

/// What installation asks for the import `id`, by the rules of `tembra_contracts`: a system
/// grant only for the interfaces `system_capability` admits, and no grant for the HTTP types of
/// a `handler_only` Orb, which planning binds to the ingress. With the HTTP client imported, the
/// client's HTTP grant serves the types too.
fn classify_import(id: &str, handler_only: bool) -> Import {
    let kind = match id.split('/').next().unwrap_or_default() {
        "wasi:filesystem" => "filesystem (FilesystemGrant)",
        "wasi:sockets" => "network (NetworkGrant)",
        "wasi:http" if handler_only && id.starts_with("wasi:http/types@") => {
            return Import::Ingress;
        }
        "wasi:http" => "http (HttpGrant)",
        package if package.starts_with("wasi:") => match system_capability(id) {
            Some(SystemCapability::Clocks) => "system: clocks",
            Some(SystemCapability::Random) => "system: random",
            Some(SystemCapability::Cli) => "system: cli",
            None => "NOT SUPPORTED by Tembra",
        },
        _ => return Import::Orb,
    };
    Import::Wasi(kind)
}

/// Exports as services and methods, with the lifetime a service chose, the incoming HTTP
/// handler, hooks declared and contributed to, kernel-managed state and resources, and imports
/// with the grant they will need at installation; an import by key with the interface whose
/// objects it reaches.
pub fn summary(resolve: &Resolve, world: WorldId, orb_name: &str, roles: &ComponentRoles) {
    let ComponentRoles {
        contributions,
        hooks,
        services,
        references,
    } = roles;
    let world = &resolve.worlds[world];
    let (mut state, mut resources) = (false, false);
    let ingress = world.exports.keys().any(|key| is_ingress(resolve, key));
    println!("exports, the services of {orb_name}:");
    for (key, item) in &world.exports {
        let id = world_key(resolve, key, None);
        if contributions.contains_key(&id) || hooks.contains_key(&id) {
            continue; // a hook callback, not a service
        }
        if is_ingress(resolve, key) {
            continue; // the incoming HTTP handler, on a line of its own
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
            let policy = match services.get(&id) {
                Some(ServicePolicy {
                    lifetime: Lifetime::Host,
                    ..
                }) => "  lifetime: host, pinned until the host closes".to_owned(),
                Some(ServicePolicy { linger_ms, .. }) if *linger_ms > 0 => {
                    format!("  linger: {linger_ms} ms after the last reference is released")
                }
                _ => String::new(),
            };
            println!(
                "  {orb_name}::{id}  methods: {}{policy}",
                methods.join(", ")
            );
        } else {
            println!("  {id}");
        }
    }
    if ingress {
        println!(
            "incoming HTTP handler: called by the host's ingress, never a portable Orb service ({HTTP_HANDLER})"
        );
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
    // An Orb serving incoming HTTP without the HTTP client gets no HTTP grant: planning binds its
    // HTTP types to the ingress.
    let handler_only = ingress
        && !world
            .imports
            .keys()
            .any(|key| world_key(resolve, key, None).starts_with("wasi:http/client@"));
    let mut requires = Vec::new();
    let mut grants: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut ingress_types = Vec::new();
    let mut context = false;
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
            context |= id == guest::CONTEXT;
            continue;
        }
        match classify_import(&id, handler_only) {
            Import::Orb => requires.push(id),
            Import::Wasi(kind) => grants.entry(kind).or_default().push(id),
            Import::Ingress => ingress_types.push(id),
        }
    }
    if !ingress_types.is_empty() {
        println!(
            "incoming HTTP types: a handler-only Orb needs no grant for them ({})",
            ingress_types.join(", ")
        );
    }
    if context {
        println!(
            "kernel-served context: the call's principal and cancellation, the object's address and the log ({})",
            guest::CONTEXT
        );
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
            match references.get(&id) {
                Some(target) => println!("  {id}  by key, the objects of {target}"),
                None => println!("  {id}"),
            }
        }
    }
    if !grants.is_empty() {
        println!("WASI imports, grants the installer must give:");
        for (kind, ids) in grants {
            println!("  {kind}: {}", ids.join(", "));
        }
    }
}

#[cfg(test)]
mod tests;
