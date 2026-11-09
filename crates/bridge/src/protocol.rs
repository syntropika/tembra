//! Closed names used only at adapter serialization seams.
//!
//! Kernel operation names are generated from the command table in `commands`, and lifecycle
//! effect, action and step names from their wire unions in `wire`. They are re-exported here.

/// Define a closed set of stable wire names with parsing and TypeScript generation support.
macro_rules! names {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
        pub enum $name { $($variant),+ }
        impl $name {
            /// Every variant name paired with its stable wire name, in declaration order.
            pub const ALL: &'static [(&'static str, &'static str)] =
                &[$((stringify!($variant), $wire)),+];
            /// Return the stable wire name for this variant.
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } }
            /// Reject wire names outside this protocol enum.
            pub fn parse(value: &str) -> Option<Self> {
                match value { $($wire => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

pub use crate::commands::KernelOp;
pub use crate::wire::{ActionType, EffectType, StepType};

names! {
    /// Stable wire names; unknown input is rejected before dispatch.
    NativeOp {
        Activate => "activate",
        Dispose => "dispose",
        CancelEffect => "cancel-effect",
        Effect => "effect",
        Invoke => "invoke",
        Contribute => "contribute",
    }
}
