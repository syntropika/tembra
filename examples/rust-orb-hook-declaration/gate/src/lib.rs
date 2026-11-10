//! A compiled Orb that declares a hook. `Guard` is the hook: the kernel composes the
//! contributions of other Orbs into its plan, and `Guard::run` runs that plan. The gate's own
//! service, `Admissions`, asks the hook what to do with each value.
#![cfg_attr(target_arch = "wasm32", no_std)]

#[tembra_orb_macro::orb(package = "example:gate@1.0.0", wit = "../wit")]
mod gate {
    /// The hook this Orb declares: a waterfall, each contributor may change the admission or
    /// block it, in plan order. Its callback is the `guard` interface of the shared WIT package.
    #[hook("example:admission/guard@1.0.0", composition = waterfall)]
    pub struct Guard;

    /// What the gate decided for one value.
    pub struct Decision {
        pub value: i32,
        pub blocked: bool,
    }

    /// The gate's service.
    #[service]
    pub struct Admissions;

    impl Admissions {
        /// Run the guard hook over a fresh admission and report what the contributors decided.
        pub fn admit(value: i32) -> Decision {
            let decided = Guard::run(guard::Admission {
                value,
                blocked: false,
            });
            Decision {
                value: decided.value,
                blocked: decided.blocked,
            }
        }
    }
}
