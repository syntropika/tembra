//! A `no_std` guest, whose module brings the freestanding runtime for wasm32: its allocator, and a
//! panic handler that the facade drops when one of its features links std.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra::orb(package = "example:greeter@1.0.0", no_std)]
pub mod greeter {
    use alloc::{format, string::String};

    /// Greets.
    #[service]
    pub struct Greeting;

    impl Greeting {
        /// Greet `name`.
        pub fn hello(name: String) -> String {
            format!("Hello, {name}!")
        }
    }
}
