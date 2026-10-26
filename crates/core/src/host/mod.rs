use crate::Reason;
mod contexts;
mod lifecycle;
mod orbs;
mod references;
mod resources;
mod tasks;
#[cfg(test)]
mod tests;

use crate::{
    Code, Error, Result,
    index::{Index, VersionCounts},
    model::*,
    program::WaitKey,
    state::Snapshot,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
};

#[derive(Clone)]
pub struct Host {
    pub(crate) inner: Rc<RefCell<Inner>>,
}

pub(crate) use storage::Inner;
use storage::Pending;

mod ownership;
mod storage;

impl Default for Host {
    /// Create a fresh platform-independent kernel.
    fn default() -> Self {
        Self::new()
    }
}

/// Describe a record that no longer exists.
pub(crate) fn gone(kind: Reason) -> Error {
    Error::new(Code::Gone, kind, format!("{kind} is no longer available"))
}

/// Describe an operation rejected by the current lifecycle state.
pub(crate) fn conflict(reason: Reason) -> Error {
    Error::new(Code::Conflict, reason, reason.as_str().replace('_', " "))
}

/// Report identity or revision exhaustion without wrapping counters.
fn exhausted() -> Error {
    Error::new(
        Code::Exhausted,
        Reason::Identities,
        "identity or revision space exhausted",
    )
}

/// Issue the next identity within the shared safe-integer limit.
fn reserve(next: &mut Id) -> Result<Id> {
    let id = *next;
    if id > MAX_ID {
        return Err(exhausted());
    }
    *next += 1;
    Ok(id)
}

/// Reject invalid declarations before constructing a replacement graph.
fn validate_orb(orb: &Orb) -> Result<()> {
    if orb.name.is_empty() || orb.api_version == 0 {
        return Err(Error::invalid(
            Reason::Orb,
            "orb name and api version are required",
        ));
    }
    if !cfg!(feature = "hooks")
        && (!orb.hooks.is_empty() || orb.requires.iter().any(|r| !r.hooks.is_empty()))
    {
        return Err(Error::new(
            Code::Unsupported,
            Reason::Hooks,
            "hooks were excluded from this build",
        ));
    }
    let mut services = BTreeSet::new();
    for service in &orb.services {
        if service.name.is_empty()
            || !services.insert(&service.name)
            || service.methods.iter().any(|m| {
                m.is_empty()
                    || matches!(
                        m.as_str(),
                        "then" | "release" | "state" | "ready" | "status" | "opaque"
                    )
            })
        {
            return Err(Error::invalid(
                Reason::Service,
                "invalid or duplicate service/member name",
            ));
        }
        if service.lifetime == Lifetime::Host && service.linger_ms != 0 {
            return Err(Error::invalid(
                Reason::Linger,
                "host-lifetime objects cannot linger",
            ));
        }
    }
    Ok(())
}
