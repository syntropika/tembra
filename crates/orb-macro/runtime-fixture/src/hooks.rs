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
            pub async fn visit(who: String, hour: u32) {
                Visits::run(Visit {
                    who,
                    hour: Hour(hour),
                }).await
            }

            /// Tell every contributor where a guest goes.
            pub async fn go(floor: u32, door: String) {
                Spots::run(places::Spot::from(places::Place { floor, door })).await
            }

            /// Tell every contributor which door a guest knocks at.
            pub async fn knock(door: String) {
                lobby::Doors::run(keys::Door::from(door)).await
            }

            /// The room the first contributor with an answer gives the floor, if any.
            pub async fn room(floor: u32) -> Option<u32> {
                Rooms::run(floor).await
            }

            /// The value the contributions admit, and whether they blocked it.
            pub async fn admit(value: i32) -> (i32, bool) {
                let admission = Guard::run(guard::Admission {
                    value,
                    blocked: false,
                }).await;
                (admission.value, admission.blocked)
            }

            /// The admission the contributions answer, in the hook's own type: the desk's
            /// interface and `Guard::run` name one `guard::Admission`.
            pub async fn admitted(admission: guard::Admission) -> guard::Admission {
                let verdict = Guard::run(admission).await;
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
            pub async fn judged(value: i32) -> guard::Verdict {
                Guard::run(guard::Admission {
                    value,
                    blocked: false,
                }).await
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
    /// A host without hooks can compile this authoring crate and refuses installation explicitly.
    #[cfg(not(feature = "native-hooks"))]
    #[test]
    fn a_hook_declaration_without_hooks_is_refused_natively() {
        assert_eq!(
            crate::native::refused(super::gatehouse::orb()),
            "native hook declarations need the runtime's `hooks`: enable the facade's `hooks` feature in the host that installs this Orb"
        );
    }

    /// Native declarations compose the same data as the component, without serde on author types.
    #[cfg(feature = "native-hooks")]
    #[test]
    fn native_declarations_compose_series_bail_and_waterfall_in_plan_order() {
        use crate::native::{
            ORB, call,
            facade::{Hook, Host, Orb, contracts::Contract, core::Requirement},
            ready,
        };
        use futures::executor::LocalPool;
        use serde_json::{Value, json};
        use std::{cell::RefCell, rc::Rc};
        let contract = Contract::from_source_in(
            concat!(env!("CARGO_MANIFEST_DIR"), "/admission"),
            "gatehouse.wit",
            super::gatehouse::WIT,
            None,
        )
        .unwrap();
        let identity = |name| format!("example:gatehouse/{name}@1.0.0");
        let signature = |hook: &str| contract.function_signature(hook, "apply", true).unwrap();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut contributor = Orb::new("watch", 1).requires(Requirement {
            orb: ORB.into(),
            api_version: 1,
            hooks: [
                identity("visits"),
                identity("spots"),
                identity("doors"),
                identity("rooms"),
                "example:admission/guard@1.0.0".into(),
            ]
            .into(),
            methods: Default::default(),
            signatures: Default::default(),
            hook_signatures: Default::default(),
        });
        for name in ["visits", "spots", "doors"] {
            let name = identity(name);
            let hook = Hook::<Value, ()>::series(&name).signature(signature(&name));
            for priority in [-1, 1] {
                let seen = seen.clone();
                contributor =
                    contributor.contribute(ORB, &hook, priority, move |_, input: Value| {
                        seen.borrow_mut().push((priority, input));
                        async { Ok(()) }
                    });
            }
        }
        let rooms = identity("rooms");
        let rooms = Hook::<Value, Option<Value>>::bail(&rooms).signature(signature(&rooms));
        contributor = contributor
            .contribute(ORB, &rooms, -1, |_, input: Value| async move {
                Ok((input.as_u64().unwrap() > 0).then_some(json!(300)))
            })
            .contribute(ORB, &rooms, 1, |_, _| async { Ok(Some(json!(0))) });
        let guard = "example:admission/guard@1.0.0";
        let guard = Hook::<Value, Value>::waterfall(guard).signature(signature(guard));
        contributor = contributor.contribute(ORB, &guard, 0, |_, input: Value| async move {
            Ok(json!({"value": input["value"].as_i64().unwrap() + 1, "blocked": true}))
        });
        let mut pool = LocalPool::new();
        let host = Host::new(pool.spawner());
        host.install(vec![super::gatehouse::orb().unwrap(), contributor])
            .unwrap();
        let desk = ready(&mut pool, &host, "example:gatehouse/desk@1.0.0");
        assert_eq!(
            call(&mut pool, &host, &desk, "alice", "visit", json!(["ada", 9])),
            Value::Null
        );
        assert_eq!(
            call(&mut pool, &host, &desk, "alice", "go", json!([3, "east"])),
            Value::Null
        );
        assert_eq!(
            call(&mut pool, &host, &desk, "alice", "knock", json!(["east"])),
            Value::Null
        );
        assert_eq!(
            *seen.borrow(),
            [
                (-1, json!({"who":"ada","hour":9})),
                (1, json!({"who":"ada","hour":9})),
                (-1, json!({"floor":3,"door":"east"})),
                (1, json!({"floor":3,"door":"east"})),
                (-1, json!("east")),
                (1, json!("east"))
            ]
        );
        assert_eq!(
            call(&mut pool, &host, &desk, "alice", "room", json!([3])),
            json!(300)
        );
        assert_eq!(
            call(&mut pool, &host, &desk, "alice", "room", json!([0])),
            json!(0)
        );
        assert_eq!(
            call(&mut pool, &host, &desk, "alice", "admit", json!([7])),
            json!([8, true])
        );
        desk.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert!(host.errors().is_empty());
    }
}
