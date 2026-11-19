#[tembra::orb(package = "example:desk@1.0.0")]
mod desk {
    pub mod tickets {
        #[state(service = Tickets)]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Count {
            pub total: u32,
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
