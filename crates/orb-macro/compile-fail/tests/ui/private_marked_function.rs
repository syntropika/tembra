#[tembra::orb(package = "example:ledger@1.0.0")]
mod ledger {
    pub mod books {
        #[resource]
        pub struct Counts {
            pub seen: u32,
        }

        impl Counts {
            #[open]
            fn open() -> Self {
                Counts { seen: 0 }
            }
        }
    }

    #[service]
    pub struct Passing;

    impl Passing {
        pub fn add(amount: i64) -> i64 {
            amount
        }
    }
}

fn main() {}
