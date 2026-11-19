#[tembra::orb(package = "example:lending@1.0.0")]
mod lending {
    pub mod setup {
        #[inputs]
        struct Lent {
            pub amount: i64,
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
