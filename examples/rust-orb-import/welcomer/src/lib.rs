//! The consumer: a compiled Orb calling another Orb. `import` on the attribute names the greeter's
//! interface, read from the contract the greeter's build wrote; the macro re-exports its generated
//! module as `greeting`, so `greeting::hello(name)` is an ordinary function call here and a call
//! through the kernel at run time. Which Orb answers is decided by the installer, not by this code.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra_orb_macro::orb(
    package = "example:welcomer@1.0.0",
    wit = "../greeter/contract",
    import = "example:greeter/greeting@1.0.0"
)]
mod welcomer {
    use alloc::{string::String, vec::Vec};

    /// The service: welcomes a list of names, one call to the greeter per name.
    #[service]
    pub struct Welcoming;

    impl Welcoming {
        /// Greet each name through the imported interface and return the greetings.
        pub fn welcome(names: Vec<String>) -> Vec<String> {
            names.iter().map(|name| greeting::hello(name)).collect()
        }
    }
}
