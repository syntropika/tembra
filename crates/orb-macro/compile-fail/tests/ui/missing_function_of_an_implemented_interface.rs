#[tembra::orb(
    package = "example:keeping@1.0.0",
    wit = "consumed/counters"
)]
mod keeping {
    pub mod keeper {
        use super::book;

        #[service("example:counters/book@1.0.0")]
        pub struct Keeper;

        impl Keeper {
            pub fn add(entry: book::Entry) -> u32 {
                entry.label.len() as u32
            }
        }

        #[wit_resource]
        pub struct Cursor;

        impl Cursor {
            #[constructor]
            pub fn new() -> Self {
                Cursor
            }
        }
    }
}

fn main() {}
