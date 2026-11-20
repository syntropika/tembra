#[tembra::orb(package = "example:context@1.0.0")]
mod context {
    pub mod sessions {
        #[wit_resource]
        pub struct Session;

        impl Session {
            pub fn read(&self, first: &tembra::Context, second: &tembra::Context) {}
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
