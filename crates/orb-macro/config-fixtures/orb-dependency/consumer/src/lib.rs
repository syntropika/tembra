//! A crate holding one Orb module, which depends on a library holding another. Natively it builds;
//! as a component it does not, since a component holds one Orb, its dependencies' included.

#[tembra::orb(package = "example:tally@1.0.0")]
pub mod tally {
    /// A counter.
    #[service]
    pub struct Counter;

    impl Counter {
        /// The provider's first count, plus `by`.
        pub fn add(by: u32) -> u32 {
            tembra_config_provider::FIRST + by
        }
    }
}
