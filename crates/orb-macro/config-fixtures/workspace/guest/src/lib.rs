//! The guest of the workspace: an Orb that declares a hook. The native side does not cover a hook
//! declaration, so with the application's `host` the module still compiles, and its `orb()` answers
//! an error naming the declaration.

#[tembra::orb(package = "example:gate@1.0.0")]
pub mod gate {
    /// The hook: a waterfall, each contributor may change the admission or block it.
    #[hook(composition = waterfall, input = Admission)]
    pub struct Guard;

    /// One admission: the value, and whether a contributor blocked it.
    pub struct Admission {
        pub value: i32,
        pub blocked: bool,
    }

    pub mod admissions {
        use super::{Admission, Guard};

        /// The gate's service.
        #[service]
        pub struct Admissions;

        impl Admissions {
            /// Run the hook over a fresh admission of the value.
            pub fn admit(value: i32) -> Admission {
                Guard::run(Admission {
                    value,
                    blocked: false,
                })
            }
        }
    }
}
