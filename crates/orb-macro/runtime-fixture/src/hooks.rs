/// Declares a hook of each composition: a series and a bail derived from their input, the series'
/// holding a newtype, and the waterfall `guard` of the package in `admission/`, declared by its
/// identity, whose callback answers a `verdict`: the admission's fields under another name. Other
/// Orbs contribute; the desk runs each hook and reports what the contributions decided.
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
