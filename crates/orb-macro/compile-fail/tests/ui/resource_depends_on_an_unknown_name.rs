#[tembra::orb(package = "example:ledger@1.0.0")]
mod ledger {
    #[resource(depends = [Totals])]
    pub struct Counts {
        pub seen: u32,
    }

    impl Counts {
        #[open]
        pub fn open() -> Self {
            Counts { seen: 0 }
        }
    }

    pub mod passing {
        #[service]
        pub struct Passing;

        impl Passing {
            pub fn add(amount: i64) -> i64 {
                amount
            }
        }
    }
}

fn main() {}
