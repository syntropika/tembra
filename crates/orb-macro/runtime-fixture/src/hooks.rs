/// Declares a hook of each composition: a series and a bail derived from their input, the series'
/// holding a newtype, and the waterfall `guard` of the package in `admission/`, declared by its
/// identity, whose callback answers a `verdict` the hook uses from the package's `types`: the
/// admission's fields under another name. Other Orbs contribute; the desk runs each hook and
/// reports what the contributions decided, keeps the last admission in its state, and names the
/// hook's types through the view of its keyed import too.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(
        package = "example:gatehouse@1.0.0",
        wit = "admission",
        import("example:admission/guard@1.0.0", keyed),
        no_std
    )
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(
        package = "example:gatehouse@1.0.0",
        wit = "admission",
        import("example:admission/guard@1.0.0", keyed)
    )
)]
pub mod gatehouse {
    use alloc::string::String;

    /// Every contributor sees each visit, in plan order.
    #[hook(composition = series, input = Visit)]
    pub struct Visits;

    /// A visit: who came, and at what hour. It holds a newtype, so it stays this struct, converted
    /// to the hook's import at `run`.
    pub struct Visit {
        pub who: String,
        pub hour: Hour,
    }

    /// An hour of the day: a newtype, which the WIT spells as its `u32`.
    pub struct Hour(u32);

    impl From<u32> for Hour {
        fn from(hour: u32) -> Self {
            Self(hour)
        }
    }

    /// Every contributor sees where a guest goes: a newtype holding a record of the module.
    #[hook(composition = series, input = places::Spot)]
    pub struct Spots;

    /// Where guests go, in a module of its own, whose `use` brings what its types hold.
    pub mod places {
        use alloc::string::String;

        /// A door of a floor.
        pub struct Place {
            pub floor: u32,
            pub door: String,
        }

        /// Where a guest goes: a newtype, which the WIT spells as its record, converted to each
        /// side of the hook's bindings.
        pub struct Spot(Place);

        impl From<Place> for Spot {
            fn from(place: Place) -> Self {
                Self(place)
            }
        }
    }

    /// A hook declared in a module that imports nothing: its input is a newtype of another module,
    /// whose `String` the hook's conversions spell through the newtype.
    pub mod lobby {
        /// Every contributor sees which door a guest knocks at.
        #[hook(composition = series, input = super::keys::Door)]
        pub struct Doors;
    }

    /// Where the newtype of a `String` lives, in a module whose `use` brings the type.
    pub mod keys {
        use alloc::string::String;

        /// The name of a door: a newtype, which the WIT spells as its `String`.
        pub struct Door(String);

        impl From<String> for Door {
            fn from(name: String) -> Self {
                Self(name)
            }
        }
    }

    /// The first contributor with an answer gives the room of a floor.
    #[hook(composition = bail, input = u32)]
    pub struct Rooms;

    /// The guard of the consumed package: each contributor may change the admission or block it.
    #[hook("example:admission/guard@1.0.0", composition = waterfall)]
    pub struct Guard;

    pub mod desk {
        use super::{
            Guard, Hour, Rooms, Spots, Visit, Visits, guard, guard_ref, keys, lobby, places,
        };
        use alloc::string::String;

        /// The last admission the desk ran the guard on, in the hook's own type.
        #[state]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Gate {
            pub last: Option<guard::Admission>,
        }

        /// Runs the hooks.
        #[service]
        pub struct Desk;

        impl Desk {
            /// Tell every contributor of a visit.
            pub fn visit(who: String, hour: u32) {
                Visits::run(Visit {
                    who,
                    hour: Hour(hour),
                })
            }

            /// Tell every contributor where a guest goes.
            pub fn go(floor: u32, door: String) {
                Spots::run(places::Spot::from(places::Place { floor, door }))
            }

            /// Tell every contributor which door a guest knocks at.
            pub fn knock(door: String) {
                lobby::Doors::run(keys::Door::from(door))
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

            /// The admission the contributions answer, in the hook's own type: the desk's
            /// interface and `Guard::run` name one `guard::Admission`.
            pub fn admitted(admission: guard::Admission) -> guard::Admission {
                let verdict = Guard::run(admission);
                let admission = guard::Admission {
                    value: verdict.value,
                    blocked: verdict.blocked,
                };
                Gate::update(|gate| {
                    gate.last = Some(guard::Admission {
                        value: admission.value,
                        blocked: admission.blocked,
                    })
                });
                admission
            }

            /// The verdict the contributions answer, in the type the hook uses from `types`.
            pub fn judged(value: i32) -> guard::Verdict {
                Guard::run(guard::Admission {
                    value,
                    blocked: false,
                })
            }

            /// The last admission `admitted` answered, which the state holds.
            pub fn last() -> Option<guard::Admission> {
                Gate::read().last
            }

            /// An admission named through the view of the hook's keyed import, which is the type
            /// the desk's interface and `Guard::run` name.
            pub fn viewed(admission: guard_ref::Admission) -> guard::Admission {
                admission
            }
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    /// Natively, the module compiles and `orb()` answers why a host cannot install it: the
    /// keyed import of the guard and a hook's `run` are synchronous, and a native method has no
    /// fiber to suspend while the kernel completes the call or runs the contributions, so only a
    /// component serves the module. The first part it names is the keyed import.
    #[test]
    fn a_hook_declaration_is_refused_natively() {
        assert_eq!(
            crate::native::refused(super::gatehouse::orb()),
            "the import of example:admission/guard@1.0.0 by key works in a component build only: its function `apply` is synchronous, and natively a method has no fiber to suspend while the kernel completes the call, so a native Orb imports only interfaces whose functions are `async func`"
        );
    }
}
