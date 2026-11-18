#[tembra::orb(package = "example:books@1.0.0")]
mod books {
    pub mod model {
        pub struct Entry {
            pub title: String,
        }
    }

    #[service]
    pub struct Shelf;

    impl Shelf {
        pub fn first() -> crate::books::model::Entry {
            crate::books::model::Entry {
                title: String::new(),
            }
        }
    }
}

fn main() {}
