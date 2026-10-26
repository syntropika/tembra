//! Stable typed identities shared by language adapters.

use super::{ContextValue, Result};
use std::marker::PhantomData;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextKind {
    Text,
    Integer,
    Boolean,
    Opaque,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextContract {
    pub name: String,
    pub version: u32,
    pub kind: ContextKind,
}

pub trait ContextDatum: Sized {
    const KIND: ContextKind;

    /// Move typed metadata into its shared representation.
    fn into_value(self) -> ContextValue;

    /// Refuse a binding with a different scalar kind.
    fn from_value(value: ContextValue) -> Result<Self>;
}

pub struct ContextKey<T> {
    name: &'static str,
    version: u32,
    marker: PhantomData<fn() -> T>,
}

impl<T: ContextDatum> ContextKey<T> {
    /// Declare a stable namespaced key without code generation.
    pub const fn new(name: &'static str, version: u32) -> Self {
        Self {
            name,
            version,
            marker: PhantomData,
        }
    }

    /// Describe this key for the shared engine.
    pub fn contract(&self) -> ContextContract {
        ContextContract {
            name: self.name.into(),
            version: self.version,
            kind: T::KIND,
        }
    }
}
