//! An integration test with an Orb module of its own, in the library's package: were it to write
//! the crate's `contract/`, it would also remove the library's contract, whose package it shares.

#[tembra::orb(package = "example:shelf@1.0.0")]
mod probe {
    /// The echo.
    #[service]
    pub struct Echo;

    impl Echo {
        /// One ping.
        pub fn ping() -> u32 {
            1
        }
    }
}

#[test]
fn the_module_of_the_test_derives_its_own_world() {
    assert_eq!(probe::Echo::ping(), 1);
    assert!(probe::WIT.contains("world probe"), "{}", probe::WIT);
}
