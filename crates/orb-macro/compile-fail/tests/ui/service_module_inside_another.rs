#[tembra::orb(package = "example:desk@1.0.0")]
mod desk {
    pub mod tickets {
        #[service]
        pub struct Tickets;

        impl Tickets {
            pub fn count() -> u32 {
                0
            }
        }

        pub mod other {
            #[service]
            pub struct Other;

            impl Other {
                pub fn count() -> u32 {
                    0
                }
            }
        }
    }
}

fn main() {}
