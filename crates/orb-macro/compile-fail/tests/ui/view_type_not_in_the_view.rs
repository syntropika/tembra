#[tembra::orb(
    package = "example:router@1.0.0",
    wit = "consumed/counters",
    import("example:counters/counter@1.0.0", keyed)
)]
mod router {
    pub mod routing {
        use super::counter_ref;

        #[service]
        pub struct Routing;

        impl Routing {
            pub fn total(count: counter_ref::Count) -> u32 {
                count.value
            }
        }
    }
}

fn main() {}
