//! An Orb crate whose manifest uses TOML 1.1 syntax. With the facade's `host`, its `orb()` names the
//! Orb as the manifest's `[package.metadata.tembra]` does, which the macro reads.

#[tembra::orb(package = "example:greeter@1.0.0")]
pub mod greeter {
    pub mod greeting {
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
}
