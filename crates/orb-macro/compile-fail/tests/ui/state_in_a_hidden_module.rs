#[tembra::orb(package = "example:ledger@1.0.0")]
mod ledger {
    pub mod accounts {
        mod books {
            #[state(service = Passing)]
            #[derive(Default, serde::Serialize, serde::Deserialize)]
            pub struct Balance {
                pub total: i64,
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
