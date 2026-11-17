//! A binary with an Orb module of its own.

#[tembra::orb(package = "example:tool@1.0.0")]
mod tool {
    /// The tool.
    #[service]
    pub struct Hammer;

    impl Hammer {
        /// One strike.
        pub fn strike() -> u32 {
            1
        }
    }
}

fn main() {
    println!(
        "{} strike, {} bytes of WIT",
        tool::Hammer::strike(),
        tool::WIT.len()
    );
}
