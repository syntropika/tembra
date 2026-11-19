//! An example, built as a binary, with an Orb module of its own. An example built as a library,
//! with `crate-type = ["lib"]`, would write the crate's `contract/`: the macro does not tell it
//! from the library today.

#[tembra::orb(package = "example:demo@1.0.0")]
mod demo {
    pub mod show {
        /// The demonstration.
        #[service]
        pub struct Show;

        impl Show {
            /// One act.
            pub fn act() -> u32 {
                1
            }
        }
    }
}

fn main() {
    println!(
        "{} act, {} bytes of WIT",
        demo::show::Show::act(),
        demo::WIT.len()
    );
}
