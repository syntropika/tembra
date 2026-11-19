#[tembra::orb(package = "example:counter@1.0.0")]
mod tally {
    pub mod counter {
        #[state]
        #[derive(serde::Serialize, serde::Deserialize)]
        pub struct Tally {
            pub total: u32,
        }

        #[service]
        pub struct Counter;

        impl Counter {
            pub fn add(by: u32) -> u32 {
                Tally::update(|tally| tally.total += by).total
            }
        }
    }
}

fn main() {}
