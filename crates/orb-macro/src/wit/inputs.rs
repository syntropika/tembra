//! Resolve canonical package roots together without creating a dependency tree on disk.
use super::{GUEST, error};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use wit_component::WitPrinter;
use wit_parser::{Resolve, UnresolvedPackageGroup};

/// Canonical packages and their authored sources, with non-standard packages nested as WIT.
pub(super) struct Composed {
    pub resolve: Resolve,
    pub inline: String,
    pub files: BTreeSet<PathBuf>,
}

/// Parse roots and their local dependencies once, then let the official resolver order them.
pub(super) fn compose(roots: &[PathBuf]) -> syn::Result<Composed> {
    let mut locations = BTreeSet::new();
    for root in roots {
        let root = root.canonicalize().map_err(|cause| failed(root, cause))?;
        locations.insert(root.clone());
        if root.is_dir() {
            let deps = root.join("deps");
            if deps.exists() {
                for entry in std::fs::read_dir(&deps).map_err(|cause| failed(&deps, cause))? {
                    let path = entry.map_err(|cause| failed(&deps, cause))?.path();
                    if path.is_dir()
                        || path.extension().is_some_and(|extension| {
                            extension == "wit" || extension == "wasm" || extension == "wat"
                        })
                    {
                        locations
                            .insert(path.canonicalize().map_err(|cause| failed(&path, cause))?);
                    }
                }
            }
        }
    }
    let mut bundled = BTreeMap::new();
    let mut groups = Vec::new();
    for (name, source) in tembra_contracts::WASI_SOURCES
        .iter()
        .copied()
        .chain([("guest.wit", GUEST)])
    {
        let group = UnresolvedPackageGroup::parse(name, source)
            .map_err(|(_, cause)| failed(Path::new(name), cause))?;
        bundled.insert(group.main.name.clone(), source);
        groups.push(group);
    }
    let mut claimed = BTreeMap::new();
    let mut files = BTreeSet::new();
    for location in locations {
        let group = if location.is_dir() {
            UnresolvedPackageGroup::parse_dir(&location)
                .map_err(|cause| failed(&location, cause))?
        } else {
            let source =
                std::fs::read_to_string(&location).map_err(|cause| failed(&location, cause))?;
            UnresolvedPackageGroup::parse(&location, &source)
                .map_err(|(_, cause)| failed(&location, cause))?
        };
        let sources: Vec<_> = group
            .source_map
            .source_files()
            .map(Path::to_path_buf)
            .collect();
        files.extend(sources.iter().cloned());
        if let Some(standard) = bundled.get(&group.main.name) {
            let exact = sources.len() == 1
                && std::fs::read_to_string(&sources[0])
                    .map_err(|cause| failed(&sources[0], cause))?
                    .replace("\r\n", "\n")
                    == standard.replace("\r\n", "\n");
            if !exact {
                return Err(error(format!(
                    "{} redefines the bundled WIT package {}; use its canonical standard contract",
                    location.display(),
                    group.main.name
                )));
            }
            continue;
        }
        for package in std::iter::once(&group.main).chain(&group.nested) {
            if bundled.contains_key(&package.name) {
                return Err(error(format!(
                    "{} redefines the bundled WIT package {}",
                    location.display(),
                    package.name
                )));
            }
            if let Some(previous) = claimed.insert(package.name.clone(), location.clone()) {
                return Err(error(format!(
                    "WIT package {} is defined by both {} and {}; name one canonical source",
                    package.name,
                    previous.display(),
                    location.display()
                )));
            }
        }
        groups.push(group);
    }
    let main = groups.pop().expect("the bundled package set is nonempty");
    let mut resolve = Resolve::default();
    resolve
        .push_groups(main, groups)
        .map_err(|cause| error(format!("cannot resolve canonical WIT packages: {cause:#}")))?;
    let mut inline = String::new();
    for (id, package) in resolve.packages.iter() {
        if bundled.contains_key(&package.name) {
            continue;
        }
        let mut printer = WitPrinter::default();
        printer
            .print_package(&resolve, id, false)
            .map_err(|cause| {
                error(format!(
                    "cannot print canonical WIT package {}: {cause:#}",
                    package.name
                ))
            })?;
        inline.push('\n');
        inline.push_str(&String::from(printer.output));
    }
    Ok(Composed {
        resolve,
        inline,
        files,
    })
}

/// Report the canonical input responsible for a parsing or filesystem error.
fn failed(path: &Path, cause: impl std::fmt::Display) -> syn::Error {
    error(format!(
        "cannot read canonical WIT package {}: {cause}",
        path.display()
    ))
}
