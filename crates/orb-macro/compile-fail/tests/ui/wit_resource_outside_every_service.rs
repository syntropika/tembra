#[tembra::orb(package = "example:desk@1.0.0")]
mod desk {
    pub mod tokens {
        #[wit_resource]
        pub struct Token;

        impl Token {
            pub fn id(&self) -> u32 {
                0
            }
        }
    }

    pub mod tickets {
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
