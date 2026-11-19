//! A binary with an Orb module of its own.

#[tembra::orb(package = "example:tool@1.0.0")]
mod tool {
    pub mod hammer {
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
}

fn main() {
    println!(
        "{} strike, {} bytes of WIT",
        tool::hammer::Hammer::strike(),
        tool::WIT.len()
    );
}
