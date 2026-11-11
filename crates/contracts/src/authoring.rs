//! WIT resolution, packaging and real compiled-component verification for native tooling.
//!
//! This module is the only user of `wit-parser` and `wit-component`. Runtime queries are answered
//! by the prepared contract it produces, so tooling and runtimes share one implementation.
use crate::{
    Artifact, Error, PreparedContract,
    artifact::{ComponentRoles, Container, Identity, append_section, encode_orb_roles},
};
use std::{collections::BTreeSet, path::Path};
use wit_parser::{
    PackageId, PackageName, Resolve, UnresolvedPackage, UnresolvedPackageGroup, WorldId,
};

/// A resolved package graph with an explicitly selected entry world.
#[derive(Clone)]
pub struct Contract {
    pub(crate) resolve: Resolve,
    pub(crate) world: WorldId,
    prepared: PreparedContract,
}

/// Every runtime query is answered by the prepared contract.
impl std::ops::Deref for Contract {
    type Target = PreparedContract;

    fn deref(&self) -> &PreparedContract {
        &self.prepared
    }
}

/// Format parser diagnostics with their full context.
fn parser(error: impl std::fmt::Display) -> Error {
    Error(format!("{error:#}"))
}

/// Parse one WIT source into its unresolved package group, rendering errors with context.
pub(crate) fn parse_source(
    path: impl AsRef<Path>,
    source: &str,
) -> Result<UnresolvedPackageGroup, Error> {
    UnresolvedPackageGroup::parse(path, source).map_err(|(map, error)| Error(error.render(&map)))
}

/// Render a resolution error against the Resolve's own (span-adjusted) source map.
fn resolution(resolve: &Resolve, error: wit_parser::ResolveError) -> Error {
    Error(resolve.render_error(&anyhow::Error::from(error)))
}

/// Name the WIT package or dependency path whose parsing failed.
fn package(path: &Path, error: impl std::fmt::Display) -> Error {
    Error(format!(
        "failed to parse WIT package {}: {error:#}",
        path.display()
    ))
}

/// Refuse a parsed package that redefines one already in the Resolve (a bundled standard
/// package or an encoded dependency) with a typed error, before wit-parser's re-add assertion.
fn redefined(
    resolve: &Resolve,
    standard: &BTreeSet<PackageName>,
    origin: &Path,
    group: &UnresolvedPackageGroup,
) -> Result<(), Error> {
    for unresolved in std::iter::once(&group.main).chain(&group.nested) {
        let name = &unresolved.name;
        if resolve.package_names.contains_key(name) {
            let owner = if standard.contains(name) {
                "the standard contracts bundled with Tembra"
            } else {
                "an encoded WIT dependency"
            };
            return Err(Error(format!(
                "WIT package {name} in {} is already provided by {owner}; remove the duplicate",
                origin.display()
            )));
        }
    }
    Ok(())
}

impl Contract {
    /// Prepare the compact runtime contract once for a resolved world. `explicit` lists the
    /// world's own imports; `None` (unknown provenance) treats every import as explicit.
    pub(crate) fn new(
        resolve: Resolve,
        world: WorldId,
        explicit: Option<&BTreeSet<String>>,
    ) -> Result<Self, Error> {
        let prepared = crate::prepare::world(&resolve, world, explicit)?;
        Ok(Self {
            resolve,
            world,
            prepared,
        })
    }

    /// Resolve a WIT directory and its local `deps/` without acquiring packages.
    ///
    /// Follows `Resolve::push_dir`: dependency directories and text files are parsed once into
    /// their public unresolved form, so the world's own imports are known before elaboration
    /// adds type-only dependencies. Encoded packages (`*.wasm`, `*.wat`, or a `*.wit` whose
    /// content is binary) are merged through `push_file` and carry no provenance.
    pub fn from_dir(path: impl AsRef<Path>, world: Option<&str>) -> Result<Self, Error> {
        let path = path.as_ref();
        let (mut resolve, mut known) = crate::standard::resolve()?;
        let standard: BTreeSet<_> = resolve.package_names.keys().cloned().collect();
        let main = UnresolvedPackageGroup::parse_dir(path).map_err(|error| package(path, error))?;
        let mut dependencies = Vec::new();
        let deps = path.join("deps");
        if deps.exists() {
            let mut entries = std::fs::read_dir(&deps)
                .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>())
                .map_err(|error| package(&deps, error))?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let dependency = entry.path();
                if dependency.is_dir() {
                    let group = UnresolvedPackageGroup::parse_dir(&dependency)
                        .map_err(|error| package(&dependency, error))?;
                    dependencies.push((dependency, group));
                    continue;
                }
                let extension = dependency
                    .extension()
                    .and_then(|extension| extension.to_str());
                if !matches!(extension, Some("wit" | "wasm" | "wat")) {
                    continue;
                }
                let bytes =
                    std::fs::read(&dependency).map_err(|error| package(&dependency, error))?;
                if extension == Some("wat") || bytes.starts_with(b"\0asm") {
                    // Encoded packages carry no import provenance; worlds that include them
                    // conservatively treat every import as explicit.
                    resolve
                        .push_file(&dependency)
                        .map_err(|error| package(&dependency, error))?;
                    continue;
                }
                let source =
                    String::from_utf8(bytes).map_err(|error| package(&dependency, error))?;
                let group = parse_source(&dependency, &source)
                    .map_err(|error| package(&dependency, error))?;
                dependencies.push((dependency, group));
            }
        }
        for (origin, group) in dependencies
            .iter()
            .map(|(origin, group)| (origin.as_path(), group))
            .chain([(path, &main)])
        {
            redefined(&resolve, &standard, origin, group)?;
            known.push(group.main.clone());
            known.extend(group.nested.iter().cloned());
        }
        let dependencies = dependencies.into_iter().map(|(_, group)| group).collect();
        let package = resolve
            .push_groups(main, dependencies)
            .map_err(|error| resolution(&resolve, error))?;
        Self::select(resolve, package, world, &known)
    }

    /// Resolve one self-contained WIT source and require selection for ambiguous worlds.
    pub fn from_source(name: &str, source: &str, world: Option<&str>) -> Result<Self, Error> {
        let (mut resolve, mut known) = crate::standard::resolve()?;
        let standard: BTreeSet<_> = resolve.package_names.keys().cloned().collect();
        let group = parse_source(name, source)?;
        redefined(&resolve, &standard, Path::new(name), &group)?;
        known.push(group.main.clone());
        known.extend(group.nested.iter().cloned());
        let package = resolve
            .push_group(group)
            .map_err(|error| resolution(&resolve, error))?;
        Self::select(resolve, package, world, &known)
    }

    /// Select the entry world, which may be package-qualified and so belong to a dependency,
    /// nested or bundled standard package, and prepare it with that world's own provenance.
    fn select(
        resolve: Resolve,
        package: PackageId,
        world: Option<&str>,
        known: &[UnresolvedPackage],
    ) -> Result<Self, Error> {
        let world = resolve.select_world(&[package], world).map_err(parser)?;
        let explicit = crate::provenance::world(&resolve, world, known);
        Self::new(resolve, world, explicit.as_ref())
    }

    /// Decode an encoded WIT package produced by `encode`.
    ///
    /// A binary WIT package has no import provenance: every interface import is treated as
    /// explicit, so callable imports conservatively require their providers and grants.
    pub fn from_bytes(bytes: &[u8], world: Option<&str>) -> Result<Self, Error> {
        let wit_component::DecodedWasm::WitPackage(resolve, package) =
            wit_component::decode(bytes).map_err(parser)?
        else {
            return Err(Error(
                "prepared contracts require an encoded WIT package".into(),
            ));
        };
        let world = resolve.select_world(&[package], world).map_err(parser)?;
        Self::new(resolve, world, None)
    }

    /// The compact runtime contract embedded in artifacts.
    pub fn prepared(&self) -> &PreparedContract {
        &self.prepared
    }

    /// Encode the resolved package and standard dependencies for in-memory binding generation.
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let package = self.resolve.worlds[self.world]
            .package
            .ok_or_else(|| Error("selected world has no package".into()))?;
        wit_component::encode(&self.resolve, package, false).map_err(parser)
    }

    /// Require a compiled component's exported interfaces and signatures to match this world.
    pub fn check_component_exports(&self, bytes: &[u8]) -> Result<(), Error> {
        verify_component(&self.prepared, bytes)
    }

    /// Compare complete exported signatures independently of resolver-local type indexes.
    pub fn check_exports(&self, other: &Self) -> Result<(), Error> {
        if self.signatures(true)? != other.signatures(true)? {
            return Err(Error(
                "component export signatures differ from the selected WIT world".into(),
            ));
        }
        Ok(())
    }

    /// Package a closed source bundle for the selected world.
    pub fn source_artifact(
        &self,
        source: &str,
        name: &str,
        api_version: u32,
    ) -> Result<Vec<u8>, Error> {
        Artifact::package_source(name, api_version, &self.prepared, source)
    }

    /// Package a component with typed roles; WIT owns every callback and view signature.
    ///
    /// Contribution roles name exported hook callbacks. Reference roles map an imported
    /// reference-view binding to the provider interface it addresses. Hook roles name the hooks
    /// the component declares, which it cannot state in WIT. Each is checked here and again
    /// whenever the artifact is decoded.
    pub fn component_artifact(
        &self,
        component: &[u8],
        name: &str,
        api_version: u32,
        roles: &ComponentRoles,
    ) -> Result<Vec<u8>, Error> {
        self.prepared.check_resource_profile()?;
        self.prepared.check_roles(roles)?;
        self.prepared.reference_bindings(&roles.references)?;
        let identity = Identity {
            name: name.into(),
            api_version,
            world: self.prepared.world().to_owned(),
        };
        identity.validate()?;
        if !Container::parse(component)?.tembra.is_empty() {
            return Err(Error(
                "component already contains Orb artifact metadata".into(),
            ));
        }
        self.check_component_exports(component)?;
        let mut bytes = component.to_vec();
        append_section(
            &mut bytes,
            "tembra:orb",
            &encode_orb_roles(&identity, &self.prepared, roles),
        );
        Ok(bytes)
    }
}

/// Require exact exports and check every retained executable import against its declared signature.
pub(crate) fn verify_component(expected: &PreparedContract, bytes: &[u8]) -> Result<(), Error> {
    let wit_component::DecodedWasm::Component(resolve, world) =
        wit_component::decode(bytes).map_err(parser)?
    else {
        return Err(Error("expected an executable WebAssembly component".into()));
    };
    // A decoded component has no import provenance; only signatures are compared here.
    let actual = crate::prepare::world(&resolve, world, None)?;
    if expected.signatures(true)? != actual.signatures(true)? {
        return Err(Error(
            "component export signatures differ from the selected WIT world".into(),
        ));
    }
    if expected.used_import_signatures(&actual)? != actual.signatures(false)? {
        return Err(Error(
            "component import signatures differ from the selected WIT world".into(),
        ));
    }
    Ok(())
}
