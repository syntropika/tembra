//! Reading a component: its contract, its core imports, and the grants its imports imply.
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
};
use tembra_contracts::{
    ComponentRoles, Contract, HTTP_HANDLER, Item, Lifetime, Model, ModuleContract, ServicePolicy,
    SystemCapability, guest, system_capability,
};
use wasmparser::{Parser, Payload};
use wit_component::{DecodedWasm, WitPrinter};
use wit_parser::{
    FunctionKind, InterfaceId, PackageId, PackageName, Resolve, WorldId, WorldItem, WorldKey,
};

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
/// A component built with the macro carries the module's own WIT in a custom section, which
/// `ModuleContract` reads; the packages it consumes are nested after it, whole, from the
/// directory the macro named when it is given, else from the component's types, which keep only
/// the functions the component calls. Any other component gets the WIT derived from its types,
/// with the given world name.
pub fn contract_text(
    bytes: &[u8],
    resolve: &Resolve,
    world: WorldId,
    name: &str,
    consumed: Option<&Path>,
) -> Result<(String, String)> {
    match ModuleContract::from_component(bytes, consumed)
        .map_err(|error| anyhow::anyhow!("{error}"))?
    {
        Some(module) => Ok((module.text().to_owned(), module.world().to_owned())),
        None => Ok((derived_wit(resolve, world, name)?, name.to_owned())),
    }
}

/// The author's package as WIT text, the consumed packages nested in it, and a world with the
/// given name listing what the component imports and exports.
pub(crate) fn derived_wit(resolve: &Resolve, world: WorldId, name: &str) -> Result<String> {
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
/// exports, importing it itself (`Model::pairs`): an import elaboration added because a view
/// `use`s its types, the view of the interface the Orb implements, pairs with no export. The
/// component's manifest says which hooks it declares, with their composition, which it
/// contributes to, with their priorities, which of its services chose a lifetime or a linger, and
/// which imports are the views of interfaces it imports by key, as `Contract::embedded_roles`
/// reads and checks them. Every other hook interface is a contribution at priority 0, as for a
/// component built without the macro, which carries no manifest.
pub fn roles(contract: &Contract, component: &[u8]) -> Result<ComponentRoles> {
    let mut roles = contract
        .embedded_roles(component)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    for id in contract.prepared().model().paired() {
        if !roles.hooks.contains_key(&id) {
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
    let model = contract
        .as_ref()
        .map(|(contract, ..)| contract.prepared().model());
    summary(
        &mut std::io::stdout(),
        &resolve,
        world,
        "orb",
        &roles,
        model,
    )?;
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

/// What installation asks for the import `id`, by the rules of `tembra_contracts`: a grant only
/// for the interfaces `host_serves` names, a system grant only for those `system_capability`
/// admits, and no grant for the HTTP types of a `handler_only` Orb, which planning binds to the
/// ingress. With the HTTP client imported, the client's HTTP grant serves the types too.
fn classify_import(id: &str, handler_only: bool) -> Import {
    let kind = match id.split('/').next().unwrap_or_default() {
        package if package.starts_with("wasi:") && !tembra_contracts::host_serves(id) => {
            "NOT SUPPORTED by Tembra"
        }
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
/// objects it reaches, and not the interface itself when the world imports it only for the types
/// the view `use`s. The imports listed are those installation binds to a provider or a grant, by
/// the rule of the contract's `model`, whether the world exports their interface or not: one with
/// a function or a resource that the world imports itself, or one elaboration added for the types
/// another import `use`s that holds a resource (`Interface::callable`), unless the contract pairs
/// it with an export as a hook (`Model::pairs`). Without the contract, an import with functions
/// is listed unless the world exports its interface besides. Written to `out`.
pub fn summary(
    out: &mut impl Write,
    resolve: &Resolve,
    world: WorldId,
    orb_name: &str,
    roles: &ComponentRoles,
    model: Option<&Model>,
) -> std::io::Result<()> {
    let ComponentRoles {
        contributions,
        hooks,
        services,
        references,
    } = roles;
    let world = &resolve.worlds[world];
    let (mut state, mut resources) = (false, false);
    let ingress = world.exports.keys().any(|key| is_ingress(resolve, key));
    writeln!(out, "exports, the services of {orb_name}:")?;
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
            resources |= id == guest::RESOURCE_INIT || id == guest::SERVICE_RESOURCE_INIT;
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
            writeln!(
                out,
                "  {orb_name}::{id}  methods: {}{policy}",
                methods.join(", ")
            )?;
        } else {
            writeln!(out, "  {id}")?;
        }
    }
    if ingress {
        writeln!(
            out,
            "incoming HTTP handler: called by the host's ingress, never a portable Orb service ({HTTP_HANDLER})"
        )?;
    }
    if !hooks.is_empty() {
        writeln!(
            out,
            "hooks declared, run by the kernel's plan of contributions:"
        )?;
        for (id, composition) in hooks {
            writeln!(
                out,
                "  {id}  {}",
                serde_json::json!(composition).as_str().unwrap_or_default()
            )?;
        }
    }
    if !contributions.is_empty() {
        writeln!(
            out,
            "hook contributions, bound to the provider with Install::requires:"
        )?;
        for (id, priority) in contributions {
            writeln!(out, "  {id}  priority {priority}")?;
        }
    }
    if state {
        writeln!(
            out,
            "kernel-managed state: the kernel keeps the state of this Orb's service objects ({})",
            guest::STATE
        )?;
    }
    if resources {
        writeln!(
            out,
            "kernel-managed resources: the kernel opens and closes the resources of this Orb's root and service objects ({})",
            guest::RESOURCE
        )?;
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
        let id = world_key(resolve, key, None);
        // Installation binds the import by the contract's rule, whether the component calls any
        // of its functions or not: to nothing when it is not callable, with neither a function
        // nor a resource, or held only for the types another import `use`s and holding no
        // resource; to the hook it declares or contributes to when the contract pairs it with an
        // export; and otherwise to the provider the installer names or to a grant. Without the
        // contract, an import with functions is callable, and paired when the world exports its
        // interface besides.
        let imported = model.and_then(|model| {
            model.imports.iter().find_map(|entry| match &entry.item {
                Item::Interface(import) if entry.key == id => Some((model, import)),
                _ => None,
            })
        });
        let (callable, paired) = match imported {
            Some((model, import)) => (import.callable(), model.pairs(import)),
            None => (
                !resolve.interfaces[*interface].functions.is_empty(),
                world.exports.contains_key(key),
            ),
        };
        if !callable {
            continue; // types only: nothing to bind
        }
        if contributions.contains_key(&id) || hooks.contains_key(&id) || guest::reserved(&id) {
            // Bound to the provider through the contribution, the entry to the Orb's own hook,
            // or served by the kernel.
            context |= id == guest::CONTEXT;
            continue;
        }
        if paired {
            // With the contract, the roles name every import it pairs, a hook. Without it, an
            // import of an interface the world exports besides may be a hook or one elaboration
            // added for the types a view `use`s, which the summary cannot tell apart: left out.
            continue;
        }
        match classify_import(&id, handler_only) {
            Import::Orb => requires.push(id),
            Import::Wasi(kind) => grants.entry(kind).or_default().push(id),
            Import::Ingress => ingress_types.push(id),
        }
    }
    if !ingress_types.is_empty() {
        writeln!(
            out,
            "incoming HTTP types: a handler-only Orb needs no grant for them ({})",
            ingress_types.join(", ")
        )?;
    }
    if context {
        writeln!(
            out,
            "kernel-served context: the call's principal and cancellation, the object's address and the log ({})",
            guest::CONTEXT
        )?;
    }
    if requires.is_empty() && grants.is_empty() {
        if contributions.is_empty() && hooks.is_empty() {
            writeln!(out, "imports: none; the installer grants nothing")?;
        } else {
            writeln!(out, "other imports: none; the installer grants nothing")?;
        }
    }
    if !requires.is_empty() {
        writeln!(out, "imports of other Orbs, bound with Install::requires:")?;
        for id in requires {
            match references.get(&id) {
                Some(target) => writeln!(out, "  {id}  by key, the objects of {target}")?,
                None => writeln!(out, "  {id}")?,
            }
        }
    }
    if !grants.is_empty() {
        writeln!(out, "WASI imports, grants the installer must give:")?;
        for (kind, ids) in grants {
            writeln!(out, "  {kind}: {}", ids.join(", "))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
