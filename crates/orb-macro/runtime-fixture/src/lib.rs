//! The same WIT implementation with std's runtime or the explicit freestanding runtime.
#![cfg_attr(all(target_arch = "wasm32", feature = "no-std"), no_std)]
extern crate alloc;

#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:runtime@1.0.0", no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:runtime@1.0.0")
)]
mod guest {
    use alloc::{format, string::String};

    #[service]
    pub struct Greeting;

    impl Greeting {
        pub fn hello(name: String) -> String {
            #[cfg(not(feature = "no-std"))]
            {
                let names = std::collections::HashMap::from([("name", name)]);
                format!("Hello, {}!", names["name"])
            }
            #[cfg(feature = "no-std")]
            {
                format!("Hello, {name}!")
            }
        }
    }
}
