//! A compiled Orb that reads the kernel's context, whose `tembra` the `host` feature may reach: a
//! guest gets it when its `tembra` keeps the default features, or from another crate of the build.
//! A build for wasm32 is a component either way, whose `context` is the bindings'. With `no-std`,
//! the crate builds for wasm32 without std, on the module's freestanding runtime.
#![cfg_attr(all(target_arch = "wasm32", feature = "no-std"), no_std)]
extern crate alloc;

#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:probe@1.0.0", context, no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:probe@1.0.0", context)
)]
mod probe {
    pub mod caller {
        use super::context;
        use alloc::string::String;

        /// The service: it reports who calls it.
        #[service]
        pub struct Caller;

        impl Caller {
            /// The principal of the call.
            pub fn principal() -> String {
                context::principal()
            }
        }
    }
}
