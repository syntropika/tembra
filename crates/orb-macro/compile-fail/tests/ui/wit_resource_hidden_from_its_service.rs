#[tembra::orb(package = "example:walk@1.0.0")]
mod walk {
    pub mod a {
        pub mod b {
            mod c {
                #[wit_resource]
                pub struct Pen {
                    pub ink: u32,
                }

                impl Pen {
                    pub fn ink(&self) -> u32 {
                        self.ink
                    }
                }
            }
        }

        #[service]
        pub struct Walker;

        impl Walker {
            pub fn lend() -> b::c::Pen {
                todo!()
            }
        }
    }
}

fn main() {}
