//! The interfaces the kernel itself serves to compiled Orbs.
//!
//! A world imports them without a provider and without a grant; the state and resource interfaces
//! come with the export that answers them, the context interface stands alone. The `tembra`
//! namespace is reserved for them; their WIT is bundled with the standard contracts, so an Orb
//! cannot redefine it.
use crate::{Item, Model};

/// Read and update the kernel-managed state of the service object running the call.
pub const STATE: &str = "tembra:guest/state@1.0.0";

/// Exported by an Orb whose services own state: the kernel asks for each object's declaration.
pub const STATE_INIT: &str = "tembra:guest/state-init@1.0.0";

/// Read the handle of one of the root's open resources.
pub const RESOURCE: &str = "tembra:guest/resources@1.0.0";

/// Exported by an Orb whose root owns resources: declarations, then open and close callbacks.
pub const RESOURCE_INIT: &str = "tembra:guest/resources-init@1.0.0";

/// The call's principal and cancellation, the address of the object running it, and the log.
pub const CONTEXT: &str = "tembra:guest/context@1.0.0";

/// The declaring callback of [`STATE_INIT`] and [`RESOURCE_INIT`].
pub const DECLARE: &str = "declare";

/// The opening callback of [`RESOURCE_INIT`].
pub const OPEN: &str = "open";

/// The closing callback of [`RESOURCE_INIT`].
pub const CLOSE: &str = "close";

/// The interfaces a world imports from the kernel that come with an export answering them.
pub const PAIRS: [(&str, &str); 2] = [(STATE, STATE_INIT), (RESOURCE, RESOURCE_INIT)];

/// Every interface the kernel serves to a world that imports it.
pub const IMPORTS: [&str; 3] = [STATE, RESOURCE, CONTEXT];

/// Whether the kernel serves an imported interface identity.
pub fn serves(contract: &str) -> bool {
    IMPORTS.contains(&contract)
}

/// Whether an interface identity belongs to the namespace reserved for kernel interfaces.
pub fn reserved(contract: &str) -> bool {
    contract.starts_with("tembra:")
}

/// Whether a world imports the interface.
pub fn imports(model: &Model, contract: &str) -> bool {
    names(&model.imports, contract)
}

/// Whether a world exports the interface.
pub fn exports(model: &Model, contract: &str) -> bool {
    names(&model.exports, contract)
}

fn names(side: &[crate::Entry], contract: &str) -> bool {
    side.iter().any(|entry| match &entry.item {
        Item::Interface(interface) => interface.contract.as_deref() == Some(contract),
        _ => false,
    })
}
