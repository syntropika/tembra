//! The provider: the greeter of `examples/rust-orb-service`, reduced to one method. It knows
//! nothing about who calls it; another Orb imports its interface and the installer binds the two.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra_orb_macro::orb(package = "example:greeter@1.0.0")]
mod greeter {
    use alloc::{format, string::String};

    /// The service another Orb imports.
    #[service]
    pub struct Greeting;

    impl Greeting {
        /// Greet by name.
        pub fn hello(name: String) -> String {
            format!("Hello, {name}!")
        }
    }
}
