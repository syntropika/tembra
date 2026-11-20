#[tembra::orb(
    package = "example:reader@1.0.0",
    wit = "consumed/protocol",
    import = "example:protocol/future-values@1.0.0"
)]
mod reader {
    pub mod probe {
        #[service]
        pub struct Probe;

        impl Probe {
            pub fn value() -> u64 {
                0
            }
        }
    }
}

fn main() {}
