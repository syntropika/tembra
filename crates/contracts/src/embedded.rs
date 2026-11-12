//! Read what a component built with `#[orb]` says about itself beyond its WIT.
//!
//! The macro writes the module's manifest into the `tembra:manifest` custom section of the core
//! module the component linker produces, as a JSON object:
//! `{"contributes": {"ns:name/hook@version": n}, "declares": {"ns:name/hook@version": "waterfall"},
//! "services": {"ns:name/service@version": {"lifetime": "host", "linger_ms": 0}}, "wit": "dir"}`.
//! Every tool that packages such a component reads its roles here, so a component packaged by
//! `cargo tembra build` and one packaged through the Node binding carry the same roles.
use crate::{
    ComponentRoles, Entry, Error, HookComposition, Item, PreparedContract, ServicePolicy,
    artifact::COMPONENT_HEADER, codec::Reader,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// The custom section of the core module holding the manifest.
const SECTION: &str = "tembra:manifest";

/// Header of a core module: magic and version 1.
const MODULE_HEADER: [u8; 8] = *b"\0asm\x01\0\0\0";

/// The manifest a component built with `#[orb]` carries: the hooks the module contributes to,
/// with their priorities, the hooks it declares, with their composition, the services whose
/// `#[service]` attribute names a lifetime or a linger, by interface identity, and the directory
/// of the WIT packages the module consumes, relative to the crate manifest, as written on the
/// attribute.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddedManifest {
    pub contributes: BTreeMap<String, i32>,
    pub declares: BTreeMap<String, HookComposition>,
    pub services: BTreeMap<String, ServicePolicy>,
    pub wit: Option<String>,
}

impl EmbeddedManifest {
    /// Read the manifest of a component. A component without the section, one built without the
    /// macro, carries an empty manifest.
    pub fn read(component: &[u8]) -> Result<Self, Error> {
        match module_section(component, SECTION)? {
            Some(data) => serde_json::from_slice(data).map_err(|error| {
                Error(format!(
                    "the {SECTION} section is not a JSON object of hook roles and service policies: {error}"
                ))
            }),
            None => Ok(Self::default()),
        }
    }
}

/// The roles the manifest of `component` gives, checked against the contract. A hook the
/// component declares or contributes to is an interface the world both imports and exports, and
/// takes one of the two roles; a service policy names an interface the world exports and does
/// not import. The contract checks the roles themselves when the component is packaged.
pub(crate) fn roles(
    contract: &PreparedContract,
    component: &[u8],
) -> Result<ComponentRoles, Error> {
    let manifest = EmbeddedManifest::read(component)?;
    let interfaces = |entries: &[Entry]| -> BTreeSet<String> {
        entries
            .iter()
            .filter_map(|entry| match &entry.item {
                Item::Interface(interface) => interface.contract.clone(),
                _ => None,
            })
            .collect()
    };
    let exported = interfaces(&contract.model().exports);
    let paired: BTreeSet<String> = interfaces(&contract.model().imports)
        .into_iter()
        .filter(|identity| exported.contains(identity))
        .collect();
    if let Some(identity) = manifest
        .declares
        .keys()
        .find(|identity| !paired.contains(*identity))
    {
        return Err(Error(format!(
            "#[hook] declares {identity}, but the contract does not import and export it"
        )));
    }
    if let Some(identity) = manifest
        .contributes
        .keys()
        .find(|identity| !paired.contains(*identity) || manifest.declares.contains_key(*identity))
    {
        return Err(Error(format!(
            "#[hook] contributes to {identity}, but the contract does not import and export it as a contribution"
        )));
    }
    if let Some(identity) = manifest
        .services
        .keys()
        .find(|identity| !exported.contains(*identity) || paired.contains(*identity))
    {
        return Err(Error(format!(
            "#[service] names a lifetime for {identity}, but the contract does not export it as a service"
        )));
    }
    Ok(ComponentRoles {
        contributions: manifest.contributes,
        references: BTreeMap::new(),
        hooks: manifest.declares,
        services: manifest.services,
    })
}

/// The custom section `name` of the first core module holding one, in the order of the binary,
/// the modules of nested components included: the component linker keeps the custom sections of
/// the compiled code in the core module it links.
fn module_section<'a>(component: &'a [u8], name: &str) -> Result<Option<&'a [u8]>, Error> {
    const CUSTOM: u8 = 0;
    const CORE_MODULE: u8 = 1;
    const COMPONENT: u8 = 4;
    // The sections left to read of each component entered, the innermost last.
    let mut components = vec![sections(component, &COMPONENT_HEADER)?];
    while let Some(sections_left) = components.last_mut() {
        if sections_left.is_empty() {
            components.pop();
            continue;
        }
        let (id, payload) = section(sections_left)?;
        match id {
            CORE_MODULE => {
                let mut module = sections(payload, &MODULE_HEADER)?;
                while !module.is_empty() {
                    let (id, payload) = section(&mut module)?;
                    if id != CUSTOM {
                        continue;
                    }
                    let mut custom = Reader::new(payload);
                    let length = custom.wasm_u32().map_err(|_| invalid())? as usize;
                    if custom.take(length).map_err(|_| invalid())? == name.as_bytes() {
                        return Ok(Some(&payload[custom.position()..]));
                    }
                }
            }
            COMPONENT => components.push(sections(payload, &COMPONENT_HEADER)?),
            _ => {}
        }
    }
    Ok(None)
}

/// A reader over the sections of a binary that starts with `header`.
fn sections<'a>(binary: &'a [u8], header: &[u8; 8]) -> Result<Reader<'a>, Error> {
    binary
        .strip_prefix(header)
        .map(Reader::new)
        .ok_or_else(invalid)
}

/// The next section of a binary: its id and its payload.
fn section<'a>(sections: &mut Reader<'a>) -> Result<(u8, &'a [u8]), Error> {
    let id = sections.byte().map_err(|_| invalid())?;
    let size = sections.wasm_u32().map_err(|_| invalid())? as usize;
    Ok((id, sections.take(size).map_err(|_| invalid())?))
}

/// The one diagnostic for bytes that are no component, whichever part is malformed.
fn invalid() -> Error {
    Error("the component is not a valid WebAssembly component".into())
}
