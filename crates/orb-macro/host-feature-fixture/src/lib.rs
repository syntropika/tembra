//! A compiled Orb that reads the kernel's context, whose `tembra` the `host` feature may reach: a
//! guest gets it when its `tembra` keeps the default features, or from another crate of the build.
//! A build for wasm32 is a component either way, whose `context` is the bindings'.

#[tembra::orb(package = "example:probe@1.0.0", context)]
mod probe {
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
