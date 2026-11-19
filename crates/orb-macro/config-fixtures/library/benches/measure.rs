//! A bench with an Orb module of its own.

#[tembra::orb(package = "example:measure@1.0.0")]
mod measure {
    pub mod scale {
        /// The scale.
        #[service]
        pub struct Scale;

        impl Scale {
            /// One weight.
            pub fn weigh() -> u32 {
                1
            }
        }
    }
}

#[test]
fn the_module_of_the_bench_derives_its_own_world() {
    assert_eq!(measure::scale::Scale::weigh(), 1);
    assert!(measure::WIT.contains("world measure"), "{}", measure::WIT);
}
