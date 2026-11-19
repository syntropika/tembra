#[tembra::orb(package = "example:keeping@1.0.0")]
mod keeping {
    use crate::util::tembra::{self};

    pub mod report {
        #[state]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Balance {
            pub total: i64,
        }

        #[service]
        pub struct Report;

        impl Report {
            pub fn count() -> u32 {
                0
            }
        }
    }
}

mod util {
    pub mod tembra {}
}

fn main() {}
