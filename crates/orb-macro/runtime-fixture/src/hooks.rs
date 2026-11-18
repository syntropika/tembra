/// Declares a hook of each composition: a series and a bail derived from their input, and the
/// waterfall `guard` of the package in `admission/`, declared by its identity. Other Orbs
/// contribute; the desk runs each hook and reports what the contributions decided.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:gatehouse@1.0.0", wit = "admission", no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:gatehouse@1.0.0", wit = "admission")
)]
pub mod gatehouse {
    use alloc::string::String;

    /// Every contributor sees each visit, in plan order.
    #[hook(composition = series, input = Visit)]
    pub struct Visits;

    /// A visit: who came, and at what hour.
    pub struct Visit {
        pub who: String,
        pub hour: u32,
    }

    /// The first contributor with an answer gives the room of a floor.
    #[hook(composition = bail, input = u32)]
    pub struct Rooms;

    /// The guard of the consumed package: each contributor may change the admission or block it.
    #[hook("example:admission/guard@1.0.0", composition = waterfall)]
    pub struct Guard;

    /// Runs the hooks.
    #[service]
    pub struct Desk;

    impl Desk {
        /// Tell every contributor of a visit.
        pub fn visit(who: String, hour: u32) {
            Visits::run(Visit { who, hour })
        }

        /// The room the first contributor with an answer gives the floor, if any.
        pub fn room(floor: u32) -> Option<u32> {
            Rooms::run(floor)
        }

        /// The value the contributions admit, and whether they blocked it.
        pub fn admit(value: i32) -> (i32, bool) {
            let admission = Guard::run(guard::Admission {
                value,
                blocked: false,
            });
            (admission.value, admission.blocked)
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    /// Natively, the module compiles and `orb()` answers why a host cannot install it: a hook's
    /// `run` is synchronous, and a native method has no fiber to suspend while the kernel runs the
    /// contributions, so only a component serves a declaration.
    #[test]
    fn a_hook_declaration_is_refused_natively() {
        assert_eq!(
            crate::native::refused(super::gatehouse::orb()),
            "the declaration of the hook example:gatehouse/visits@1.0.0 works in a component build only: its `run` is synchronous, and natively a method has no fiber to suspend while the kernel runs the hook's contributions, so a native Orb declares no hook"
        );
    }
}
