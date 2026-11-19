#[tembra::orb(package = "example:desk@1.0.0")]
mod desk {
    pub mod tickets {
        pub mod gate {
            #[hook(composition = series, input = u32)]
            pub struct Guard;
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
