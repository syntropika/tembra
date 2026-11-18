//! A library whose other targets hold Orb modules of their own: an integration test, a bench, a
//! binary and an example. Only the library's module is the crate's Orb, so only it writes the
//! crate's `contract/`.

#[tembra::orb(package = "example:shelf@1.0.0")]
pub mod shelf {
    /// The shelf.
    #[service]
    pub struct Books;

    impl Books {
        /// How many books the shelf holds.
        pub fn count() -> u32 {
            3
        }
    }
}
