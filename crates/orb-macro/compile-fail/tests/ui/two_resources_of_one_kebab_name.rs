#[tembra::orb(package = "example:dock@1.0.0")]
mod dock {
    #[resource]
    pub struct DbPool {
        pub size: u32,
    }

    impl DbPool {
        #[open]
        pub fn open() -> Self {
            DbPool { size: 0 }
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
        pub struct DBPool {
            pub size: u32,
        }

        impl DBPool {
            #[open]
            pub fn open() -> Self {
                DBPool { size: 0 }
            }
        }
    }
}

fn main() {}
