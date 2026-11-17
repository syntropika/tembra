//! The contract of a module, as an Orb crate carries it in `contract/`.
//!
//! `#[orb]` derives the WIT of a module and writes the contract when Cargo compiles the crate for
//! itself; `cargo tembra build` reads the same WIT back from the component and writes the same
//! files. Both go through [`ModuleContract`], so a native-only Orb has the contract a compiled
//! consumer reads, and a compiled one the same whichever tool wrote it last.
use crate::{Contract, Error, authoring::parse_source};
use std::path::{Path, PathBuf};
use std::time::Duration;
use wit_component::{DecodedWasm, WitPrinter};
use wit_parser::{PackageId, PackageName, Resolve, UnresolvedPackageGroup};

/// The directory of an Orb crate holding its contract. Not `wit/`: wit-bindgen loads a directory
/// of that name inside the crate on its own and finds the package twice.
const DIRECTORY: &str = "contract";

/// The directory under it holding the standard packages the contract depends on, where
/// wit-parser looks for the packages a directory names.
const DEPENDENCIES: &str = "deps";

/// The custom section in which the macro stores the WIT it derived from the module.
const WIT_SECTION: &str = "tembra:wit";

/// The contract of a module: the WIT `#[orb]` derived from it, the packages the module consumes
/// nested whole after it in the order the WIT names them, and the standard packages it depends
/// on, which are not nested. It is read on its own before anything is written, so no other file
/// of `contract/` takes part in it, and checked to resolve with its standard packages alone.
pub struct ModuleContract {
    text: String,
    world: String,
    package: PackageName,
    contract: Contract,
    dependencies: Vec<(&'static str, &'static str)>,
}

/// The files of `<crate>/contract/` after a write.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WrittenContract {
    /// `contract/<world>.wit`.
    pub contract: PathBuf,
    /// `contract/deps/<package>.wit`, one per standard package the contract depends on.
    pub dependencies: Vec<PathBuf>,
    /// The contracts of the world's former names, removed.
    pub former_contracts: Vec<PathBuf>,
    /// The standard packages no contract of the directory depends on any more, removed.
    pub former_dependencies: Vec<PathBuf>,
}

impl ModuleContract {
    /// The contract of a module from the WIT the macro derived, with the packages the module
    /// consumes nested whole from `consumed`, the directory the module named: a provider's
    /// interface keeps the functions the module never calls, so the contract names the same
    /// interface the provider exports. A package a nested one builds on is nested too.
    pub fn new(wit: &str, consumed: Option<&Path>) -> Result<Self, Error> {
        Self::nested(wit, consumed, None)
    }

    /// The contract a component built with the macro carries: the WIT of its `tembra:wit`
    /// section, with the consumed packages nested whole from `consumed` when it is given, else
    /// from the component's types, which keep only the functions the component calls. `None` for
    /// a component built without the macro, which carries no such section.
    pub fn from_component(
        component: &[u8],
        consumed: Option<&Path>,
    ) -> Result<Option<Self>, Error> {
        let Some(wit) = crate::embedded::module_section(component, WIT_SECTION)? else {
            return Ok(None);
        };
        let wit = std::str::from_utf8(wit)
            .map_err(|_| Error(format!("the {WIT_SECTION} section is not UTF-8")))?;
        let resolve = match wit_component::decode(component).map_err(parser)? {
            DecodedWasm::Component(resolve, _) => resolve,
            DecodedWasm::WitPackage(..) => {
                return Err(Error("the file is a WIT package, not a component".into()));
            }
        };
        Self::nested(wit, consumed, Some(&resolve)).map(Some)
    }

    fn nested(wit: &str, consumed: Option<&Path>, types: Option<&Resolve>) -> Result<Self, Error> {
        let group = parse_source("contract.wit", wit)?;
        let package = group.main.name.clone();
        let world = match group.main.worlds.iter().collect::<Vec<_>>()[..] {
            [(_, world)] => escaped(&world.name),
            [] => return Err(Error("the module's WIT declares no world".into())),
            _ => {
                return Err(Error(
                    "the module's WIT declares more than one world".into(),
                ));
            }
        };
        let whole = match consumed {
            Some(dir) => {
                let mut whole = Resolve::default();
                whole.push_dir(dir).map_err(|error| {
                    Error(format!(
                        "reading the consumed WIT in {}: {error:#}",
                        dir.display()
                    ))
                })?;
                Some(whole)
            }
            None => None,
        };
        let own = package.to_string();
        let named = group.main.foreign_deps.keys().cloned().collect();
        let mut text = wit.to_owned();
        for (source, id) in consumed_packages(&own, named, whole.as_ref(), types) {
            let mut printer = WitPrinter::default();
            printer.print_package(source, id, false).map_err(parser)?;
            text.push('\n');
            text.push_str(&String::from(printer.output));
        }
        let file = format!("{DIRECTORY}/{world}.wit");
        let contract = Contract::from_source(&file, &text, Some(&world))?;
        let dependencies = contract.standard_dependencies();
        resolves_alone(&file, &text, &dependencies)?;
        Ok(Self {
            text,
            world,
            package,
            contract,
            dependencies,
        })
    }

    /// The contract as WIT text: the module's own package, the consumed packages nested after it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The name of the module's world as WIT writes it, which names the contract's file: with the
    /// `%` escape for a keyword of WIT, `contract/%list.wit` for a world `list`.
    pub fn world(&self) -> &str {
        &self.world
    }

    /// The contract as read from the text, with the world selected.
    pub fn contract(&self) -> &Contract {
        &self.contract
    }

    /// Write the contract into `<crate_dir>/contract/`: `<world>.wit` holds the text, and
    /// `deps/` the standard packages it depends on, so the directory resolves on its own, with
    /// `Resolve::push_dir` as another Orb's bindings read it and with [`Contract::from_dir`].
    /// A file whose content the directory holds already is left alone; any other is written
    /// through a temporary file renamed into place, so a reader never sees a partial file, and
    /// the stale temporaries of earlier writes are removed. The contract of a former name of the
    /// world, a `.wit` file of the same package, is removed. The directory is the crate's, shared
    /// by every Orb module of the crate, so a standard package leaves `deps/` only when no
    /// `<world>.wit` of the directory depends on it; any other file stays. Builds of one crate
    /// may write at once: a file another one removed first is no error.
    pub fn write(&self, crate_dir: &Path) -> Result<WrittenContract, Error> {
        let dir = crate_dir.join(DIRECTORY);
        let contract = dir.join(format!("{}.wit", self.world));
        std::fs::create_dir_all(&dir).map_err(|error| io("creating", &dir, error))?;
        remove_stale_temporaries(&dir)?;
        // Removed before the new file is written: where file names ignore case, a former name
        // that differs only in case is the new file.
        let former_contracts = former_contracts(&dir, &contract, &self.package)?;
        for former in &former_contracts {
            remove(former)?;
        }
        write_if_changed(&contract, &self.text)?;
        let (dependencies, former_dependencies) =
            write_dependencies(&dir, &contract, &self.dependencies)?;
        Ok(WrittenContract {
            contract,
            dependencies,
            former_contracts,
            former_dependencies,
        })
    }
}

/// Format wit-parser and wit-component diagnostics with their full context.
fn parser(error: impl std::fmt::Display) -> Error {
    Error(format!("{error:#}"))
}

/// An I/O failure on a file of the contract directory.
fn io(action: &str, path: &Path, error: std::io::Error) -> Error {
    Error(format!("{action} {}: {error}", path.display()))
}

/// A world's name as WIT writes it, and as a world specifier must name it: with WIT's `%` escape
/// when the bare name does not read as a name, a keyword of WIT such as `list`.
fn escaped(name: &str) -> String {
    match wit_parser::parse_use_path(name) {
        Ok(_) => name.to_owned(),
        Err(_) => format!("%{name}"),
    }
}

/// Whether a package is in a namespace of the standard packages that ship with tembra-contracts,
/// which a contract never nests.
fn standard(name: &PackageName) -> bool {
    matches!(name.namespace.as_str(), "wasi" | "tembra")
}

/// The packages the module's own package `own` consumes, each with the resolve to print it from:
/// the consumed directory `whole` when it holds the package, else the component's `types`, which
/// keep only the functions the component calls. In the order the module's WIT `named` them, then
/// any other these packages build on, as a package printed whole may name one the component never
/// reaches. The WIT is the one text the macro and the build share, so the two write one order;
/// the component's types, which meet the world's imports before the packages its exports use,
/// are a source only. A package found nowhere, an import the linker dropped because the code
/// never calls it, is left to the reading of the contract, which names it.
fn consumed_packages<'a>(
    own: &str,
    named: Vec<PackageName>,
    whole: Option<&'a Resolve>,
    types: Option<&'a Resolve>,
) -> Vec<(&'a Resolve, PackageId)> {
    // Each package wanted, by name, with the resolve that names it.
    let mut wanted: Vec<(PackageName, Option<&'a Resolve>)> =
        named.into_iter().map(|name| (name, types)).collect();
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
            .chain(named_by)
            .find_map(|source| Some((source, *source.package_names.get(&name)?)))
        else {
            continue;
        };
        packages.push((source, id));
        wanted.extend(
            source
                .package_direct_deps(id)
                .map(|dependency| (source.packages[dependency].name.clone(), Some(source))),
        );
    }
    packages
}

/// Check that the contract directory resolves on its own, as `Resolve::push_dir` reads it: the
/// contract `text`, to be written as `file`, and the standard packages it depends on, to be
/// written into `deps/`.
fn resolves_alone(file: &str, text: &str, dependencies: &[(&str, &str)]) -> Result<(), Error> {
    let main = parse_source(file, text)?;
    let dependencies = dependencies
        .iter()
        .map(|(name, source)| parse_source(format!("{DIRECTORY}/{DEPENDENCIES}/{name}"), source))
        .collect::<Result<Vec<_>, Error>>()?;
    let mut resolve = Resolve::default();
    resolve
        .push_groups(main, dependencies)
        .map_err(|error| Error(resolve.render_error(&anyhow::Error::from(error))))?;
    Ok(())
}

/// How old a temporary of this writer is when it is stale: a write and its rename take
/// milliseconds, so a temporary older than this was left by a build killed between the two, and
/// would stay forever, to be committed with the contract. A younger one may be a concurrent
/// writer's, a check of the crate on another target directory, and stays.
const STALE: Duration = Duration::from_secs(60);

/// Remove the stale temporaries of this writer in `dir`: the `.<name>.wit.<pid>.tmp` files
/// modified more than [`STALE`] ago. Any other file stays.
fn remove_stale_temporaries(dir: &Path) -> Result<(), Error> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).map_err(|error| io("reading", dir, error))? {
        let entry = entry.map_err(|error| io("reading", dir, error))?;
        let path = entry.path();
        let temporary = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(temporary_name);
        let stale = temporary
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= STALE);
        if stale {
            remove(&path)?;
        }
    }
    Ok(())
}

/// Whether a file name is one of this writer's temporaries, `.<name>.wit.<pid>.tmp`, as
/// [`write_if_changed`] names them.
fn temporary_name(name: &str) -> bool {
    name.strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
        .and_then(|name| name.rsplit_once('.'))
        .is_some_and(|(file, pid)| {
            file.ends_with(".wit")
                && !pid.is_empty()
                && pid.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Write `content` into `file` unless the file holds it already, through a temporary file
/// `.<name>.<pid>.tmp` in the same directory renamed into place.
fn write_if_changed(file: &Path, content: &str) -> Result<(), Error> {
    match std::fs::read(file) {
        Ok(current) if current == content.as_bytes() => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io("reading", file, error)),
    }
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error(format!("{} has no file name", file.display())))?;
    let temporary = file.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, content).map_err(|error| io("writing", &temporary, error))?;
    std::fs::rename(&temporary, file).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        io("writing", file, error)
    })
}

/// The contracts an earlier write left in `dir` under a former name of the world: the `.wit`
/// files other than `current` whose own package is `own`. A file declaring another package, or
/// one that does not parse, is not this tool's output and stays.
fn former_contracts(dir: &Path, current: &Path, own: &PackageName) -> Result<Vec<PathBuf>, Error> {
    let mut formers = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| io("reading", dir, error))? {
        let path = entry.map_err(|error| io("reading", dir, error))?.path();
        if path.file_name() == current.file_name()
            || path.extension().is_none_or(|extension| extension != "wit")
            || !path.is_file()
        {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        if UnresolvedPackageGroup::parse(&path, &source).is_ok_and(|group| group.main.name == *own)
        {
            formers.push(path);
        }
    }
    formers.sort();
    Ok(formers)
}

/// Write the standard packages the contract `current` of `dir` depends on into its `deps/`, one
/// file each, and return the files, then the files removed: a standard package an earlier write
/// left there that neither this contract nor another `<world>.wit` of `dir` depends on is removed
/// first, and `deps/` too when nothing else is left in it.
fn write_dependencies(
    dir: &Path,
    current: &Path,
    dependencies: &[(&str, &str)],
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), Error> {
    let deps = dir.join(DEPENDENCIES);
    let files: Vec<PathBuf> = dependencies
        .iter()
        .map(|(name, _)| deps.join(name))
        .collect();
    remove_stale_temporaries(&deps)?;
    // Listed before the other contracts are read: a build writes its contract before its
    // packages, so a package listed here has its contract in place for the reading.
    let candidates = former_dependencies(&deps, &files)?;
    let others = others_dependencies(dir, current)?;
    let formers: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|former| !others.iter().any(|name| deps.join(name) == *former))
        .collect();
    for former in &formers {
        remove(former)?;
    }
    if !files.is_empty() {
        std::fs::create_dir_all(&deps).map_err(|error| io("creating", &deps, error))?;
    }
    for ((_, source), file) in dependencies.iter().zip(&files) {
        write_if_changed(file, source)?;
    }
    if deps.is_dir()
        && std::fs::read_dir(&deps)
            .map_err(|error| io("reading", &deps, error))?
            .next()
            .is_none()
    {
        // Another build may have removed it first, or written into it since.
        match std::fs::remove_dir(&deps) {
            Err(error)
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                return Err(io("removing", &deps, error));
            }
            _ => {}
        }
    }
    Ok((files, formers))
}

/// The standard packages the other contracts of `dir` depend on, by their file names in `deps/`:
/// those of every `<world>.wit` but `current` that reads as the contract of its world, as
/// another Orb module of the crate writes it: the file's stem names the world as WIT writes it,
/// `%list` for a keyword of WIT. A file that does not is not this writer's, and keeps nothing.
fn others_dependencies(dir: &Path, current: &Path) -> Result<Vec<&'static str>, Error> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| io("reading", dir, error))? {
        let path = entry.map_err(|error| io("reading", dir, error))?.path();
        if path.file_name() == current.file_name()
            || path.extension().is_none_or(|extension| extension != "wit")
            || !path.is_file()
        {
            continue;
        }
        let (Some(world), Ok(source)) = (
            path.file_stem().and_then(|stem| stem.to_str()),
            std::fs::read_to_string(&path),
        ) else {
            continue;
        };
        let Ok(contract) = Contract::from_source(&path.to_string_lossy(), &source, Some(world))
        else {
            continue;
        };
        names.extend(
            contract
                .standard_dependencies()
                .into_iter()
                .map(|(name, _)| name),
        );
    }
    Ok(names)
}

/// Remove a file of the contract directory, which another build of the crate may have removed
/// first: then there is nothing left to do.
pub(crate) fn remove(path: &Path) -> Result<(), Error> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(io("removing", path, error))
        }
        _ => Ok(()),
    }
}

/// The standard packages an earlier write left in `deps` and this one does not: the `.wit` files
/// other than `current` whose package is a standard one, of the `wasi` or `tembra` namespace. A
/// file of another package, or one that does not parse, is not this tool's output and stays.
fn former_dependencies(deps: &Path, current: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    if !deps.is_dir() {
        return Ok(Vec::new());
    }
    let mut formers = Vec::new();
    for entry in std::fs::read_dir(deps).map_err(|error| io("reading", deps, error))? {
        let path = entry.map_err(|error| io("reading", deps, error))?.path();
        if current.contains(&path)
            || path.extension().is_none_or(|extension| extension != "wit")
            || !path.is_file()
        {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        if UnresolvedPackageGroup::parse(&path, &source)
            .is_ok_and(|group| standard(&group.main.name))
        {
            formers.push(path);
        }
    }
    formers.sort();
    Ok(formers)
}
