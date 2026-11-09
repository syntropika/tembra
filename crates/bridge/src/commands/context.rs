//! Context commands transport typed metadata and owner-issued opaque frame identities only.

use super::{Handle, Kernel, frame, input};
use crate::wire::{ContextEntry, ContextScalar, Ident};
use tembra_core::{ContextKind, ContextValue, Error, Id, Reason, Result};

/// Recheck a root and, when supplied, that the invocation belongs to it.
pub(super) fn check(kernel: &Kernel, root: Handle, call: Option<Handle>) -> Result<()> {
    kernel.host.check_root(root.0)?;
    foreign(kernel, root.0, call)
}

/// Refuse an invocation identity issued under another root.
fn foreign(kernel: &Kernel, root: Id, call: Option<Handle>) -> Result<()> {
    if let Some(call) = call
        && kernel.host.invocation(call.0)?.root != root
    {
        return Err(Error::invalid(Reason::Context, "foreign invocation"));
    }
    Ok(())
}

/// Derive one binding. Scalar values must match the key kind; opaque values must come from an
/// owner-issued source frame of the same root.
pub(super) fn derive(kernel: &Kernel, input: input::ContextDerive) -> Result<Id> {
    let host = &kernel.host;
    let root = input.root.0;
    let parent = frame(input.context);
    let key = input.key.contract();
    foreign(kernel, root, input.call)?;
    let value = match (key.kind, input.value, input.source) {
        (ContextKind::Text, Some(ContextScalar::Text(value)), None) => {
            ContextValue::Text(value.into())
        }
        (ContextKind::Text, _, None) => {
            return Err(Error::invalid(Reason::Metadata, "value must be text"));
        }
        (ContextKind::Integer, Some(ContextScalar::Integer(value)), None) => {
            ContextValue::Integer(value)
        }
        (ContextKind::Integer, _, None) => {
            return Err(Error::invalid(Reason::Context, "expected a safe integer"));
        }
        (ContextKind::Boolean, Some(ContextScalar::Boolean(value)), None) => {
            ContextValue::Boolean(value)
        }
        (ContextKind::Boolean, _, None) => {
            return Err(Error::invalid(Reason::Context, "expected a boolean"));
        }
        (ContextKind::Opaque, None, Some(source)) => host
            .context_value(root, source.0, &key)?
            .filter(|value| matches!(value, ContextValue::Opaque(_)))
            .ok_or_else(|| {
                Error::invalid(
                    Reason::Context,
                    "opaque references must be issued by their owner",
                )
            })?,
        (ContextKind::Opaque, _, _) => {
            return Err(Error::invalid(
                Reason::Context,
                "opaque references must be issued by their owner",
            ));
        }
        (_, _, Some(_)) => {
            return Err(Error::invalid(
                Reason::Context,
                "only opaque keys accept a source frame",
            ));
        }
    };
    host.derive_context(root, parent, input.call.map(|call| call.0), key, value)
}

/// Read the nearest binding. Opaque bindings are returned as their issuing frame identity.
pub(super) fn value(kernel: &Kernel, input: input::ContextValue) -> Result<Option<ContextEntry>> {
    let root = input.root.0;
    let at = frame(input.context);
    let key = input.key.contract();
    foreign(kernel, root, input.call)?;
    Ok(match kernel.host.context_value(root, at, &key)? {
        None => None,
        Some(ContextValue::Text(value)) => Some(ContextEntry::Text {
            value: value.to_string(),
        }),
        Some(ContextValue::Integer(value)) => Some(ContextEntry::Integer { value }),
        Some(ContextValue::Boolean(value)) => Some(ContextEntry::Boolean { value }),
        Some(ContextValue::Opaque(_)) => Some(ContextEntry::Opaque { source: Ident(at) }),
    })
}
