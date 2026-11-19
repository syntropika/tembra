#[tembra::orb(package = "example:dock@1.0.0")]
mod dock {
    #[resource]
    pub struct Mooring {
        pub berths: u32,
    }

    impl Mooring {
        #[open]
        pub fn open() -> Self {
            Mooring { berths: 0 }
        }
    }

    pub mod ship {
        #[service]
        pub struct Ship;

        impl Ship {
            pub fn berth() -> u32 {
                0
            }
        }

        #[resource]
        pub struct Mooring {
            pub berth: u32,
        }

        impl Mooring {
            #[open]
            pub fn open() -> Self {
                Mooring { berth: 0 }
            }
        }
    }
}

fn main() {}
