#[tembra::orb(
    package = "example:router@1.0.0",
    wit = "consumed/counters",
    import("example:counters/counter@1.0.0", keyed)
)]
mod proxy {
    pub mod counting {
        use super::counter;

        #[service("example:counters/counter@1.0.0")]
        pub struct Counter;

        impl Counter {
            pub async fn read() -> counter::Tally {
                counter::Tally {
                    key: Vec::new(),
                    count: counter::Count { value: 0 },
                }
            }
        }
    }

    pub mod relay {
        use super::counter_ref;

        #[service]
        pub struct Relay;

        impl Relay {
            pub fn relayed(last: counter_ref::Tally) -> u32 {
                last.count.value
            }
        }
    }
}

fn main() {}
