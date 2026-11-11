//! Prepared source Orbs: the private `tembra:orb` world and verification of real components.
//!
//! A source Orb is a component built from JavaScript against the public contract plus the private
//! `tembra:orb/source` world. Its artifact carries a `tembra:runtime` section naming that private
//! package. Verification never trusts the section: it splits the actual component's WIT into the
//! public contract, the private package, self-imports of the Orb's own hook interfaces and the
//! fixed runtime internals, and checks each part separately.
#[cfg(feature = "authoring")]
use crate::{
    Contract, Error, PreparedContract,
    artifact::{Container, Identity, append_section, encode_orb},
    model::{Entry, Item},
};
#[cfg(feature = "authoring")]
use std::collections::BTreeSet;

/// The private package version this host links.
pub const SOURCE_PACKAGE: &str = "tembra:orb@0.1.0";
/// The lifecycle callbacks every prepared source Orb exports.
pub const LIFECYCLE: &str = "tembra:orb/lifecycle@0.1.0";
/// The kernel operations a prepared source Orb may import.
pub const HOST: &str = "tembra:orb/host@0.1.0";
/// Custom section marking an artifact built against [`SOURCE_PACKAGE`].
pub(crate) const RUNTIME_SECTION: &str = "tembra:runtime";
/// The private WIT package, bundled so builds and hosts agree on one definition.
pub const SOURCE_WIT: &str = include_str!("../wit/tembra/orb.wit");
#[cfg(feature = "authoring")]
/// Package of the generated world that adds the private package to a public world.
const PREPARED_PACKAGE: &str = "tembra:prepared@0.1.0";

/// WASI 0.2 interfaces the QuickJS runtime's C library may import. The host provides them from a
/// fixed internal context with no environment, arguments, preopens or network; they never reach
/// the installation's grants. User contracts select WASI 0.3, so these never mask a user import.
pub const RUNTIME_INTERNALS: [&str; 18] = [
    "wasi:cli/environment",
    "wasi:cli/exit",
    "wasi:cli/stdin",
    "wasi:cli/stdout",
    "wasi:cli/stderr",
    "wasi:cli/terminal-input",
    "wasi:cli/terminal-output",
    "wasi:cli/terminal-stdin",
    "wasi:cli/terminal-stdout",
    "wasi:cli/terminal-stderr",
    "wasi:clocks/wall-clock",
    "wasi:clocks/monotonic-clock",
    "wasi:filesystem/types",
    "wasi:filesystem/preopens",
    "wasi:io/error",
    "wasi:io/poll",
    "wasi:io/streams",
    "wasi:random/random",
];

/// Whether an import key names one of the runtime's WASI 0.2 internals.
pub fn runtime_internal(key: &str) -> bool {
    key.split_once("@0.2.").is_some_and(|(interface, patch)| {
        RUNTIME_INTERNALS.contains(&interface) && patch.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(feature = "authoring")]
/// Prepare the immutable bundled world once; actual Orb imports and exports are checked anew.
fn private_world() -> Result<PreparedContract, Error> {
    static WORLD: std::sync::LazyLock<Result<PreparedContract, Error>> =
        std::sync::LazyLock::new(|| {
            let contract = Contract::from_source("tembra-orb.wit", SOURCE_WIT, Some("source"))?;
            Ok(contract.prepared().clone())
        });
    WORLD.clone()
}

#[cfg(feature = "authoring")]
impl Contract {
    /// The world a source Orb is componentized against: this world, the private
    /// `tembra:orb/source` world and an import of each exported hook interface named in `hooks`
    /// (JavaScript aliases), through which the Orb runs its own hooks with typed values.
    ///
    /// Use it only for building. Artifacts embed the public contract, never this world.
    pub fn source_world(&self, hooks: &BTreeSet<String>) -> Result<Contract, Error> {
        let mut resolve = self.resolve.clone();
        if resolve
            .package_names
            .keys()
            .any(|name| name.namespace == "tembra")
        {
            return Err(Error(
                "the tembra namespace is reserved for private Tembra packages".into(),
            ));
        }
        let group = crate::authoring::parse_source("tembra-orb.wit", SOURCE_WIT)?;
        resolve
            .push_group(group)
            .map_err(|error| Error(error.to_string()))?;
        let world = &resolve.worlds[self.world];
        let package = world
            .package
            .map(|package| &resolve.packages[package].name)
            .ok_or_else(|| Error("selected world has no package".into()))?;
        let version = package
            .version
            .as_ref()
            .map(|version| format!("@{version}"))
            .unwrap_or_default();
        let mut source = format!(
            "package {PREPARED_PACKAGE};\nworld source {{\n  include {}:{}/{}{version};\n  include tembra:orb/source@0.1.0;\n",
            package.namespace, package.name, world.name
        );
        let imported = self
            .model()
            .imports
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<BTreeSet<_>>();
        for alias in hooks {
            let (interface, _) = self.hook_interface(alias)?;
            if !imported.contains(interface) {
                source.push_str(&format!("  import {interface};\n"));
            }
        }
        source.push_str("}\n");
        let group = crate::authoring::parse_source("tembra-prepared.wit", &source)?;
        let package = resolve
            .push_group(group)
            .map_err(|error| Error(error.to_string()))?;
        let world = resolve
            .select_world(&[package], Some("source"))
            .map_err(|error| Error(format!("{error:#}")))?;
        Contract::new(resolve, world, None)
    }

    /// Check a componentized source Orb against this public contract (see [`verify_source`]).
    pub fn check_source_component(&self, bytes: &[u8]) -> Result<(), Error> {
        verify_source(self.prepared(), bytes)
    }

    /// Package a componentized source Orb. Its implementation is described by the component's
    /// own `lifecycle.describe` export and checked by the host before any factory runs.
    ///
    /// The component must already import the pinned shared runtime ([`crate::SharedRuntime::pack`]);
    /// [`Self::check_source_component`] also verifies a componentizer's output before packing.
    pub fn source_component_artifact(
        &self,
        component: &[u8],
        name: &str,
        api_version: u32,
    ) -> Result<Vec<u8>, Error> {
        self.prepared().check_resource_profile()?;
        let identity = Identity {
            name: name.into(),
            api_version,
            world: self.prepared().world().to_owned(),
        };
        identity.validate()?;
        if !Container::parse(component)?.tembra.is_empty() {
            return Err(Error(
                "component already contains Orb artifact metadata".into(),
            ));
        }
        self.check_source_component(component)?;
        crate::artifact::runtime_reference(true, component)?;
        let mut bytes = component.to_vec();
        append_section(
            &mut bytes,
            "tembra:orb",
            &encode_orb(&identity, self.prepared()),
        );
        append_section(&mut bytes, RUNTIME_SECTION, SOURCE_PACKAGE.as_bytes());
        Ok(bytes)
    }
}

#[cfg(feature = "authoring")]
/// Verify an actual source component without trusting its metadata:
/// - exports are exactly the public exports plus [`LIFECYCLE`], each with its declared signature;
/// - private imports are a used subset of [`HOST`];
/// - an import of one of the Orb's own exported interfaces has the export's exact signatures;
/// - remaining WASI 0.2 imports are [`RUNTIME_INTERNALS`], typed later by the host linker;
/// - every other import is a used subset of the public contract's imports.
pub(crate) fn verify_source(expected: &PreparedContract, bytes: &[u8]) -> Result<(), Error> {
    let actual = crate::authoring::decode_component(bytes)?;
    let mut public = actual.model().clone();
    let mut private = actual.model().clone();
    let is_private = |entry: &Entry| entry.key.starts_with("tembra:orb/");
    private.imports.retain(is_private);
    private.exports.retain(is_private);
    public.exports.retain(|entry| !is_private(entry));
    let exported = expected
        .model()
        .exports
        .iter()
        .map(|entry| entry.key.as_str())
        .collect::<BTreeSet<_>>();
    let mut hooks = Vec::new();
    public.imports.retain(|entry| {
        if is_private(entry) || runtime_internal(&entry.key) {
            return false;
        }
        if exported.contains(entry.key.as_str()) && !expected.imports_key(&entry.key) {
            hooks.push(entry.clone());
            return false;
        }
        true
    });
    if private.exports.iter().all(|entry| entry.key != LIFECYCLE) {
        return Err(Error(format!("a source component must export {LIFECYCLE}")));
    }
    let bundled = private_world()?;
    let private = PreparedContract::new(private)?;
    if bundled.signatures(true)? != private.signatures(true)? {
        return Err(Error(format!(
            "component lifecycle differs from the bundled {SOURCE_PACKAGE} package"
        )));
    }
    if bundled.used_import_signatures(&private)? != private.signatures(false)? {
        return Err(Error(format!(
            "component host imports differ from the bundled {SOURCE_PACKAGE} package"
        )));
    }
    for entry in hooks {
        let Item::Interface(interface) = &entry.item else {
            return Err(Error(format!(
                "{}: a hook import must be an interface",
                entry.key
            )));
        };
        for function in &interface.functions {
            if actual.function_signature(&entry.key, &function.name, false)?
                != expected.function_signature(&entry.key, &function.name, true)?
            {
                return Err(Error(format!(
                    "{}: a hook import differs from the exported interface",
                    entry.key
                )));
            }
        }
    }
    crate::authoring::compare_world(expected, &PreparedContract::new(public)?)
}

#[cfg(feature = "authoring")]
impl PreparedContract {
    /// Whether the world imports this binding key.
    fn imports_key(&self, key: &str) -> bool {
        self.model().imports.iter().any(|entry| entry.key == key)
    }
}
