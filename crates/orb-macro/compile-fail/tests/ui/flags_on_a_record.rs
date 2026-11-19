#[tembra::orb(package = "example:plotter@1.0.0")]
mod plotter {
    pub mod drawing {
        #[flags]
        pub struct Pen {
            pub filled: bool,
            pub dashed: bool,
        }

        #[service]
        pub struct Drawing;

        impl Drawing {
            pub fn current() -> Pen {
                Pen {
                    filled: true,
                    dashed: false,
                }
            }
        }
    }
}

fn main() {}
