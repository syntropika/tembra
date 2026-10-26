//! Immutable frames retained by their creating operation and live consumers.

use super::{ContextContract, ContextValue, invalid};
use crate::{Code, Error, Host, Id, Reason, Result};

pub(crate) struct ContextFrame {
    pub root: Id,
    pub parent: Id,
    pub depth: usize,
    references: usize,
    pub key: ContextContract,
    pub value: ContextValue,
}

impl crate::host::Inner {
    /// Retain a nonempty frame for a call or child frame.
    pub(crate) fn retain_frame(&mut self, frame: Id) {
        if frame != 0 {
            self.context_frames
                .get_mut(&frame)
                .expect("validated frame")
                .references += 1;
        }
    }

    /// Return opaque leases for destruction after the kernel borrow is released.
    pub(crate) fn release_frame(
        &mut self,
        mut id: Id,
        leases: &mut Vec<crate::OpaqueContextValue>,
    ) {
        while id != 0 {
            let frame = self.context_frames.get_mut(&id).expect("retained frame");
            frame.references -= 1;
            if frame.references != 0 {
                break;
            }
            let frame = self.context_frames.remove(&id).expect("unreferenced frame");
            self.roots
                .get_mut(&frame.root)
                .expect("frame retains root")
                .contexts
                .remove(&id);
            if let ContextValue::Opaque(lease) = frame.value {
                leases.push(lease);
            }
            id = frame.parent;
        }
    }

    /// Require a frame owned by this root, or the empty frame.
    pub(crate) fn check_frame(&self, root: Id, frame: Id) -> Result<()> {
        if frame != 0
            && self
                .context_frames
                .get(&frame)
                .is_none_or(|value| value.root != root)
        {
            return Err(invalid(
                "context frame belongs to another root or has expired",
            ));
        }
        Ok(())
    }
}

impl Host {
    /// Derive a frame retained by its creator and admitted consumers.
    pub fn derive_context(
        &self,
        root: Id,
        parent: Id,
        call: Option<Id>,
        key: ContextContract,
        value: ContextValue,
    ) -> Result<Id> {
        let mut inner = self.inner.borrow_mut();
        inner.check_root(root)?;
        inner.check_frame(root, parent)?;
        if call.is_some_and(|id| inner.calls.get(&id).is_none_or(|value| value.root != root)) {
            return Err(invalid("originating invocation has ended"));
        }
        let depth = if parent == 0 {
            1
        } else {
            inner.context_frames[&parent].depth + 1
        };
        if depth > 64 || inner.roots[&root].contexts.len() >= 4096 {
            return Err(Error::new(
                Code::Exhausted,
                Reason::Context,
                "context depth or frame limit reached",
            ));
        }
        if key.name.is_empty()
            || key.name.len() > 256
            || key.version == 0
            || key.kind != value.kind()
            || matches!(&value, ContextValue::Text(value) if value.len() > 4096)
            || matches!(&value, ContextValue::Integer(value) if !(-9_007_199_254_740_991..=9_007_199_254_740_991).contains(value))
        {
            return Err(invalid("invalid context contract or value"));
        }
        let identity = (key.name.clone(), key.version);
        if inner
            .context_contracts
            .get(&identity)
            .is_some_and(|kind| *kind != key.kind)
        {
            return Err(invalid("incompatible context key contract"));
        }
        if !inner.context_contracts.contains_key(&identity) && inner.context_contracts.len() >= 1024
        {
            return Err(Error::new(
                Code::Exhausted,
                Reason::Context,
                "context key limit reached",
            ));
        }
        let id = inner.id()?;
        inner.retain_frame(parent);
        if let Some(call) = call {
            inner.call_contexts.entry(call).or_default().push(id);
        }
        inner.context_contracts.insert(identity, key.kind);
        inner.context_frames.insert(
            id,
            ContextFrame {
                root,
                parent,
                depth,
                references: 1,
                key,
                value,
            },
        );
        inner
            .roots
            .get_mut(&root)
            .expect("checked root")
            .contexts
            .insert(id);
        inner.context_peak = inner.context_peak.max(inner.context_frames.len());
        Ok(id)
    }

    /// Read the nearest binding and refuse a mismatched contract.
    pub fn context_value(
        &self,
        root: Id,
        mut frame: Id,
        key: &ContextContract,
    ) -> Result<Option<ContextValue>> {
        let inner = self.inner.borrow();
        inner.check_root(root)?;
        inner.check_frame(root, frame)?;
        while frame != 0 {
            let node = &inner.context_frames[&frame];
            if node.key.name == key.name && node.key.version == key.version {
                if node.key.kind != key.kind {
                    return Err(invalid("incompatible context key contract"));
                }
                return Ok(Some(node.value.clone()));
            }
            frame = node.parent;
        }
        Ok(None)
    }

    /// Read live frames, peak frames and retained key contracts.
    pub fn context_stats(&self) -> (usize, usize, usize) {
        let inner = self.inner.borrow();
        (
            inner.context_frames.len(),
            inner.context_peak,
            inner.context_contracts.len(),
        )
    }
}
