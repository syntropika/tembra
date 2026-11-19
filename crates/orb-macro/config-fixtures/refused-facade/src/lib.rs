//! A `no_std` guest whose attribute names its facade `my-sdk`, which is no Rust identifier: the
//! macro refuses the attribute, and the build reports that refusal alone, not a path to a facade it
//! cannot resolve.
#![cfg_attr(target_arch = "wasm32", no_std)]

#[sdk::orb(package = "example:greeter@1.0.0", crate = "my-sdk", no_std)]
pub mod greeter {
    pub mod greeting {
        /// Greets.
        #[service]
        pub struct Greeting;

        impl Greeting {
            /// The greeting.
            pub fn hello() -> u32 {
                1
            }
        }
    }
}
