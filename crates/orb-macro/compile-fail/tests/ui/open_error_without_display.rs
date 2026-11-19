#[tembra::orb(package = "example:ledger@1.0.0")]
mod ledger {
    /// Why the counts did not open, which has no `Display`.
    pub struct Closed;

    #[resource]
    pub struct Counts {
        pub seen: u32,
    }

    impl Counts {
        #[open]
        pub fn open() -> Result<Self, Closed> {
            Err(Closed)
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
