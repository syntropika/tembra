#[tembra::orb(
    package = "example:router@1.0.0",
    wit = "consumed/counters",
    import("example:counters/handles@1.0.0", keyed)
)]
mod router {
    pub mod routing {
        #[service]
        pub struct Routing;

        impl Routing {
            pub fn total() -> u32 {
                0
            }
        }
    }
}

fn main() {}
