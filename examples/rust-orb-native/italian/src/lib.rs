//! The other Orb implementing `example:greeter/greeting@1.0.0`, the interface of the WIT package
//! the example shares: this one greets in Italian. `cargo tembra build` compiles the crate into a
//! component; a native host that depends on it installs `italian::orb()` instead. The crate depends
//! on the facade under another name, `sdk`, which the attribute gives with `crate`.

#[sdk::orb(package = "example:italian@1.0.0", wit = "../wit", crate = "sdk")]
pub mod italian {
    /// Implements `greeting` of the shared package.
    #[service("example:greeter/greeting@1.0.0")]
    pub struct Greeting;

    impl Greeting {
        /// Greet by name.
        pub fn hello(name: String) -> String {
            format!("Ciao, {name}!")
        }
    }
}
