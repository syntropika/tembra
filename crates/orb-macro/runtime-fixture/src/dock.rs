use alloc::string::String;

/// Note what a resource went through. A component writes it to the host's log, since its `context`
/// serves a resource's functions; natively the note goes to the journal the native tests read.
fn note(event: String) {
    #[cfg(target_arch = "wasm32")]
    dock::context::log(dock::context::Level::Info, &event);
    #[cfg(not(target_arch = "wasm32"))]
    JOURNAL.with(|journal| journal.borrow_mut().push(event));
}

#[cfg(not(target_arch = "wasm32"))]
std::thread_local! {
    /// What the resources of the Orbs this thread hosts went through, natively, in order.
    static JOURNAL: core::cell::RefCell<alloc::vec::Vec<String>> = const {
        core::cell::RefCell::new(alloc::vec::Vec::new())
    };
}

/// A resource of the Orb's root, the harbor, and two of each object of the service `Ship`, in the
/// service's module: its mooring, `#[resource(depends = [Harbor])]`, whose `#[open]` function takes
/// the next berth of the harbor and whose `#[close]` function gives it back, and its line,
/// `#[resource(depends = [Mooring])]`, whose `#[open]` function reads the mooring's berth. The
/// kernel opens the harbor at activation, and a ship's mooring, then its line, when it creates the
/// ship, and closes them in reverse when the ship goes. The harbor has two berths: the third
/// ship's mooring takes berth 3, where no line reaches, so its line answers `Err`, which fails
/// that ship's creation, and the kernel closes the mooring it opened. Beside its resources, a ship
/// owns a state, its trips, and hands out a WIT resource, its manifest, whose method reads the
/// ship's mooring.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:dock@1.0.0", context, no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:dock@1.0.0", context)
)]
pub mod dock {
    use alloc::format;

    /// The berths, how many ships moor now, and how many berths were issued.
    #[resource]
    pub struct Harbor {
        pub berths: u32,
        pub moored: u32,
        pub issued: u32,
    }

    impl Harbor {
        #[open]
        pub fn open() -> Harbor {
            super::note("open harbor".into());
            Harbor {
                berths: 2,
                moored: 0,
                issued: 0,
            }
        }

        #[close]
        pub fn close(self) {
            super::note(format!("close harbor, {} issued", self.issued));
        }
    }

    pub mod ship {
        use super::Harbor;
        use alloc::{format, string::String};

        /// Moors at a berth of the harbor, tied with a line.
        #[service]
        pub struct Ship;

        impl Ship {
            /// The berth this ship moors at.
            pub fn berth() -> u32 {
                Mooring::with(|mooring| mooring.berth)
            }

            /// Tie `knots` more in this ship's line, and answer how many it has.
            pub fn tie(knots: u32) -> u32 {
                Line::with(|line| {
                    line.knots += knots;
                    line.knots
                })
            }

            /// Sail once: count the trip in this ship's state, and answer the trip's number with
            /// the knots of the line.
            pub fn sail() -> (u32, u32) {
                let trips = Trips::update(|trips| trips.count += 1);
                (trips.count, Line::with(|line| line.knots))
            }
        }

        /// The trips of one ship, kept by the kernel beside the ship's resources.
        #[state]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Trips {
            pub count: u32,
        }

        /// A manifest of a ship, handed to callers, which names the berth of the ship it was made
        /// on.
        #[wit_resource]
        pub struct Manifest {
            pub label: String,
        }

        impl Manifest {
            #[constructor]
            pub fn new(label: String) -> Self {
                Manifest { label }
            }

            /// The label, at the berth of the ship this manifest was made on.
            pub fn describe(&self) -> String {
                format!("{} at berth {}", self.label, Mooring::with(|mooring| mooring.berth))
            }
        }

        /// The berth of one ship, the next the harbor issues when the kernel creates the ship, and
        /// given back when the ship goes.
        #[resource(depends = [Harbor])]
        pub struct Mooring {
            pub berth: u32,
        }

        impl Mooring {
            #[open]
            pub fn open() -> Mooring {
                let berth = Harbor::with(|harbor| {
                    harbor.issued += 1;
                    harbor.moored += 1;
                    harbor.issued
                });
                super::super::note(format!("open mooring {berth}"));
                Mooring { berth }
            }

            #[close]
            pub fn close(self) {
                let moored = Harbor::with(|harbor| {
                    harbor.moored -= 1;
                    harbor.moored
                });
                super::super::note(format!("close mooring {}, {moored} moored", self.berth));
            }
        }

        /// The line of one ship, which reaches the berths of the harbor alone.
        #[resource(depends = [Mooring])]
        pub struct Line {
            pub berth: u32,
            pub knots: u32,
        }

        impl Line {
            #[open]
            pub fn open() -> Result<Line, String> {
                let berth = Mooring::with(|mooring| mooring.berth);
                if berth > Harbor::with(|harbor| harbor.berths) {
                    return Err(format!("no line reaches berth {berth}"));
                }
                super::super::note(format!("open line at berth {berth}"));
                Ok(Line { berth, knots: 0 })
            }

            #[close]
            pub fn close(self) {
                super::super::note(format!("close line at berth {}", self.berth));
            }
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::{JOURNAL, dock};
    use crate::native::{call, hosting};
    use futures::FutureExt;
    use serde_json::json;
    use std::rc::Rc;
    use tembra::{core::Code, portable::Owned};

    /// What the resources went through since the last call.
    fn journal() -> Vec<String> {
        JOURNAL.with(|journal| journal.take())
    }

    /// Natively, the host opens a ship's mooring and line when it creates the ship, after the
    /// harbor, each reading what it depends on with `with`, and closes them in reverse when the
    /// ship goes; each ship reads its own, in its methods, its state's updates and the methods of
    /// the manifests it hands out; the third ship's line fails, which fails the ship with
    /// `unavailable/resource` and closes its mooring.
    #[test]
    fn a_service_resource_opens_and_closes_per_object_natively() {
        let (mut pool, host) = hosting(dock::orb().unwrap());
        let ship = |key: &str| {
            Rc::new(
                host.get(
                    "tembra-orb-runtime-fixture::example:dock/ship@1.0.0",
                    vec![key.into()],
                    "alice",
                )
                .unwrap(),
            )
        };
        let one = ship("one");
        pool.run_until(one.ready()).unwrap();
        let two = ship("two");
        pool.run_until(two.ready()).unwrap();
        assert_eq!(
            journal(),
            [
                "open harbor",
                "open mooring 1",
                "open line at berth 1",
                "open mooring 2",
                "open line at berth 2",
            ]
        );
        assert_eq!(call(&mut pool, &host, &one, "alice", "berth", json!([])), 1);
        assert_eq!(call(&mut pool, &host, &two, "alice", "berth", json!([])), 2);
        assert_eq!(call(&mut pool, &host, &one, "alice", "tie", json!([3])), 3);
        assert_eq!(call(&mut pool, &host, &one, "alice", "tie", json!([2])), 5);
        assert_eq!(call(&mut pool, &host, &two, "alice", "tie", json!([1])), 1);
        assert_eq!(
            call(&mut pool, &host, &one, "alice", "sail", json!([])),
            json!([1, 5])
        );
        assert_eq!(
            call(&mut pool, &host, &one, "alice", "sail", json!([])),
            json!([2, 5])
        );
        assert_eq!(
            call(&mut pool, &host, &two, "alice", "sail", json!([])),
            json!([1, 1])
        );
        // A manifest made on the second ship names its berth, from the ship's mooring.
        let manifest = two.clone();
        let described: String = pool
            .run_until(host.root("alice", move |context| {
                async move {
                    let manifest: Owned = manifest
                        .call_portable(&context, "[constructor]manifest", ("cargo".to_owned(),))
                        .await?;
                    manifest
                        .call_portable(&context, "[method]manifest.describe", json!([]))
                        .await
                }
                .boxed_local()
            }))
            .unwrap();
        assert_eq!(described, "cargo at berth 2");
        let three = ship("three");
        let refused = pool
            .run_until(three.ready())
            .expect_err("the third ship's line reaches no berth");
        assert_eq!(
            (
                refused.code,
                refused.reason.as_str(),
                refused.message.as_str()
            ),
            (
                Code::Unavailable,
                "resource",
                "the resource `line` did not open: no line reaches berth 3"
            ),
            "{refused}"
        );
        assert_eq!(journal(), ["open mooring 3", "close mooring 3, 2 moored"]);
        one.release().unwrap();
        pool.run_until(host.settled());
        assert_eq!(
            journal(),
            ["close line at berth 1", "close mooring 1, 1 moored"]
        );
        assert_eq!(call(&mut pool, &host, &two, "alice", "tie", json!([1])), 2);
        two.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(
            journal(),
            [
                "close line at berth 2",
                "close mooring 2, 0 moored",
                "close harbor, 3 issued",
            ]
        );
        assert!(host.errors().is_empty(), "{:?}", host.errors());
    }
}
