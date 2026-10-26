//! Scalar metadata and owner-issued leases.

use super::{ContextDatum, ContextKind, Result, invalid};
use crate::Id;
use std::{any::Any, fmt, rc::Rc};

#[derive(Clone, Debug, PartialEq)]
pub enum ContextValue {
    Text(Rc<str>),
    Integer(i64),
    Boolean(bool),
    Opaque(OpaqueContextValue),
}

impl ContextValue {
    /// Read the scalar kind without inspecting owner data.
    pub fn kind(&self) -> ContextKind {
        match self {
            Self::Text(_) => ContextKind::Text,
            Self::Integer(_) => ContextKind::Integer,
            Self::Boolean(_) => ContextKind::Boolean,
            Self::Opaque(_) => ContextKind::Opaque,
        }
    }
}

macro_rules! datum {
    ($ty:ty, $kind:ident, $variant:ident) => {
        impl ContextDatum for $ty {
            const KIND: ContextKind = ContextKind::$kind;

            /// Move the value into shared context metadata.
            fn into_value(self) -> ContextValue {
                ContextValue::$variant(self)
            }

            /// Decode only the declared scalar kind.
            fn from_value(value: ContextValue) -> Result<Self> {
                match value {
                    ContextValue::$variant(value) => Ok(value),
                    _ => Err(invalid("context type mismatch")),
                }
            }
        }
    };
}
datum!(Rc<str>, Text, Text);

impl ContextDatum for String {
    const KIND: ContextKind = ContextKind::Text;

    /// Move the value into shared context metadata.
    fn into_value(self) -> ContextValue {
        ContextValue::Text(self.into())
    }

    /// Decode only the declared scalar kind.
    fn from_value(value: ContextValue) -> Result<Self> {
        match value {
            ContextValue::Text(value) => Ok(value.to_string()),
            _ => Err(invalid("context type mismatch")),
        }
    }
}

datum!(i64, Integer, Integer);
datum!(bool, Boolean, Boolean);

/// Retain an owner-issued lease until its frame and physical consumers settle.
#[derive(Clone)]
pub struct OpaqueContextValue {
    pub id: Id,
    lease: Rc<dyn Any>,
}

impl OpaqueContextValue {
    /// Wrap an owner reference without interpreting its lease.
    pub fn new(id: Id, lease: impl Any) -> Self {
        Self {
            id,
            lease: Rc::new(lease),
        }
    }
}

impl fmt::Debug for OpaqueContextValue {
    /// Format the identity without exposing the lease.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OpaqueContextValue").field(&self.id).finish()
    }
}

impl PartialEq for OpaqueContextValue {
    /// Match both the owner identity and the issued lease.
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && Rc::ptr_eq(&self.lease, &other.lease)
    }
}

impl ContextDatum for OpaqueContextValue {
    const KIND: ContextKind = ContextKind::Opaque;

    /// Move the value into shared context metadata.
    fn into_value(self) -> ContextValue {
        ContextValue::Opaque(self)
    }

    /// Decode only the declared scalar kind.
    fn from_value(value: ContextValue) -> Result<Self> {
        match value {
            ContextValue::Opaque(value) => Ok(value),
            _ => Err(invalid("context type mismatch")),
        }
    }
}
