#[tembra::orb(package = "example:keeping@1.0.0")]
mod keeping {
    #[service]
    pub struct Report;

    impl Report {
        pub fn count() -> u32 {
            0
        }
    }

    pub mod exports {}
}

fn main() {}
