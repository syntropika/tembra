//! A compiled Orb written from its code: the module is the contract, and `cargo tembra build`
//! derives the WIT from the component.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra_orb_macro::orb(package = "example:greeter@1.0.0")]
mod greeter {
    use alloc::{format, string::String};

    /// Counts over a text.
    pub struct Stats {
        pub words: u32,
        pub chars: u32,
    }

    /// The service: a unit struct marked #[service], with an impl block of public functions.
    #[service]
    pub struct Greeting;

    impl Greeting {
        /// Greet by name.
        pub fn hello(name: String) -> String {
            format!("Hello, {name}!")
        }

        /// Count the words of a text.
        pub fn count_words(text: String) -> u32 {
            text.split_whitespace().count() as u32
        }

        /// Words and characters of a text.
        pub fn measure(text: String) -> Stats {
            Stats {
                words: Self::count_words(text.clone()),
                chars: text.chars().count() as u32,
            }
        }
    }
}
