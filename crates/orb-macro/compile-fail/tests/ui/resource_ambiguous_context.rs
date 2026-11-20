#[tembra::orb(package = "example:context@1.0.0")]
mod context {
    pub mod sessions {
        pub struct Context {
            pub principal: String,
        }

        #[wit_resource]
        pub struct Session;

        impl Session {
            pub fn read(&self, context: &Context) {}
        }

        #[service]
        pub struct Sessions;

        impl Sessions {
            pub fn open() -> Session {
                Session
            }
        }
    }
}

fn main() {}
