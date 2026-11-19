#[tembra::orb(package = "example:desk@1.0.0")]
mod desk {
    #[inputs]
    pub struct Inputs;

    pub mod tickets {
        #[root]
        fn start(
            _inputs: &super::Inputs,
            _declaration: &tembra::Declaration,
        ) -> tembra::Result<()> {
            Ok(())
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
