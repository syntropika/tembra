//! A compiled Orb that declares a hook from its own code. `Guard` is the hook: the macro derives
//! its interface, `example:gate/guard@1.0.0`, from the struct and from the type of its input, and
//! the kernel composes the contributions of other Orbs into its plan; `Guard::run` runs that plan.
//! The gate's own service, `Admissions`, asks the hook what to do with each value.
#![cfg_attr(target_arch = "wasm32", no_std)]

#[tembra_orb_macro::orb(no_std, package = "example:gate@1.0.0")]
mod gate {
    /// The hook this Orb declares: a waterfall, each contributor may change the admission or
    /// block it, in plan order. Its interface is derived from this struct and from `Admission`.
    #[hook(composition = waterfall, input = Admission)]
    pub struct Guard;

    /// One admission: the value, and whether a contributor blocked it.
    pub struct Admission {
        pub value: i32,
        pub blocked: bool,
    }

    /// The gate's service.
    #[service]
    pub struct Admissions;

    impl Admissions {
        /// Run the guard hook over a fresh admission and report what the contributors decided.
        pub fn admit(value: i32) -> Admission {
            Guard::run(Admission {
                value,
                blocked: false,
            })
        }
    }
}
