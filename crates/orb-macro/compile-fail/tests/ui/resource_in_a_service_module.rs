#[tembra::orb(package = "example:desk@1.0.0")]
mod desk {
    pub mod tickets {
        #[resource]
        pub struct Counts;

        impl Counts {
            #[open]
            fn open() -> Self {
                Counts
            }
        }

        #[service]
        pub struct Tickets;

        impl Tickets {
            pub fn count() -> u32 {
                0
            }
        }
    }
}

fn main() {}
