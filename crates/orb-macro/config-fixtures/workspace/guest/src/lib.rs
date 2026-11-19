//! A shared hook declarer compiles when Cargo enables the host without its hooks feature.
//! Its generated orb() refuses installation explicitly in that configuration.

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
            pub async fn admit(value: i32) -> Admission {
                Guard::run(Admission {
                    value,
                    blocked: false,
                }).await
            }
        }
    }
}
