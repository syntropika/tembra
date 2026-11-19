#[tembra::orb(package = "example:books@1.0.0")]
mod books {
    pub mod shelf {
        pub mod model {
            pub struct Entry {
                pub title: String,
            }
        }

        use self::model::*;

        #[service]
        pub struct Shelf;

        impl Shelf {
            pub fn first() -> Entry {
                Entry {
                    title: String::new(),
                }
            }
        }
    }
}

fn main() {}
