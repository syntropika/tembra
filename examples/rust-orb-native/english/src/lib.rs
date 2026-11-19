//! One of two Orbs implementing `example:greeter/greeting@1.0.0`, the interface of the WIT package
//! the example shares: this one greets in English. `cargo tembra build` compiles the crate into a
//! component; a native host that depends on it installs `english::orb()` instead.

#[tembra::orb(package = "example:english@1.0.0", wit = "../wit")]
pub mod english {
    pub mod greeter {
        /// Implements `greeting` of the shared package.
        #[service("example:greeter/greeting@1.0.0")]
        pub struct Greeting;

        impl Greeting {
            /// Greet by name.
            pub fn hello(name: String) -> String {
                format!("Hello, {name}!")
            }
        }
    }
}
