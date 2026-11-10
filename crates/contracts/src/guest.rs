//! The interfaces the kernel itself serves to compiled Orbs.
//!
//! A world imports them without a provider and without a grant, and exports the matching
//! callbacks. The `tembra` namespace is reserved for them; their WIT is bundled with the standard
//! contracts, so an Orb cannot redefine it.

/// Read and update the kernel-managed state of the service object running the call.
pub const STATE: &str = "tembra:guest/state@1.0.0";

/// Exported by an Orb whose services own state: the kernel asks for each object's declaration.
pub const STATE_INIT: &str = "tembra:guest/state-init@1.0.0";

/// The callback of [`STATE_INIT`]: the declaration of one service's state, if it owns one.
pub const DECLARE: &str = "declare";

/// Whether an interface identity belongs to the namespace reserved for kernel interfaces.
pub fn reserved(contract: &str) -> bool {
    contract.starts_with("tembra:")
}

/// Whether a world imports [`STATE`].
pub fn imports_state(model: &crate::Model) -> bool {
    names(model, true, STATE)
}

/// Whether a world exports [`STATE_INIT`].
pub fn exports_state_init(model: &crate::Model) -> bool {
    names(model, false, STATE_INIT)
}

fn names(model: &crate::Model, imports: bool, contract: &str) -> bool {
    let side = if imports {
        &model.imports
    } else {
        &model.exports
    };
    side.iter().any(|entry| match &entry.item {
        crate::Item::Interface(interface) => interface.contract.as_deref() == Some(contract),
        _ => false,
    })
}
