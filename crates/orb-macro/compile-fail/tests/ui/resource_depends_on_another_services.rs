#[tembra::orb(package = "example:dock@1.0.0")]
mod dock {
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

    pub mod tug {
        #[service]
        pub struct Tug;

        impl Tug {
            pub fn pull() -> u32 {
                0
            }
        }

        #[resource(depends = [Mooring])]
        pub struct Line;

        impl Line {
            #[open]
            pub fn open() -> Self {
                Line
            }
        }
    }
}

fn main() {}
