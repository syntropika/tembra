#[tembra::orb(package = "example:books@1.0.0")]
mod books {
    pub mod shelf {
        pub mod model {
            pub struct Entry {
                pub title: String,
            }
        }

        #[service]
        pub struct Shelf;

        impl Shelf {
            pub fn first() -> crate::books::shelf::model::Entry {
                crate::books::shelf::model::Entry {
                    title: String::new(),
                }
            }
        }
    }
}

fn main() {}
