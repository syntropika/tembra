//! A library holding one Orb module, whose other items a consumer uses.

/// The number of books a shelf starts with, which the consumer reads.
pub const FIRST: u32 = 3;

#[tembra::orb(package = "example:shelf@1.0.0")]
pub mod shelf {
    /// The shelf.
    #[service]
    pub struct Books;

    impl Books {
        /// How many books the shelf holds.
        pub fn count() -> u32 {
            super::FIRST
        }
    }
}
