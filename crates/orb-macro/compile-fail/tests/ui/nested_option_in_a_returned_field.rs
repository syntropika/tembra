#[tembra::orb(package = "example:ledger@1.0.0")]
mod ledger {
    pub mod book {
        pub struct Found {
            pub at: Option<Option<u32>>,
        }

        #[service]
        pub struct Book;

        impl Book {
            pub fn last() -> Option<Found> {
                None
            }
        }
    }
}

fn main() {}
