#[tembra::orb(package = "example:desk@1.0.0")]
mod desk {
    pub mod tickets {
        #[inputs]
        pub struct Inputs;

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
