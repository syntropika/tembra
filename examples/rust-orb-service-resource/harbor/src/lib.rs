//! A compiled Orb whose service objects own resources. The harbor is a resource of the Orb's root,
//! opened once at activation; each `ship` object owns a mooring, opened when the kernel creates
//! the ship and closed when it goes. The mooring depends on the harbor: its `#[open]` function
//! takes the harbor's next berth, and its `#[close]` function gives it back. The resources log
//! what they go through, through the kernel's context, so that the host sees the kernel's order
//! of opens and closes.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra::orb(no_std, package = "example:harbor@1.0.0", context)]
pub mod port {
    /// The harbor: a resource of the Orb's root, since it sits outside every service's module.
    #[resource]
    pub struct Harbor {
        /// How many berths there are.
        pub berths: u32,
        /// How many ships moor now.
        pub moored: u32,
        /// How many berths were issued so far, which numbers them.
        pub issued: u32,
    }

    impl Harbor {
        #[open]
        pub fn open() -> Harbor {
            context::log(context::Level::Info, "open harbor");
            Harbor {
                berths: 2,
                moored: 0,
                issued: 0,
            }
        }

        #[close]
        pub fn close(self) {
            context::log(context::Level::Info, "close harbor");
        }
    }

    pub mod ship {
        use super::{Harbor, context};
        use alloc::{format, string::String};

        /// A ship, which moors at a berth of the harbor.
        #[service]
        pub struct Ship;

        impl Ship {
            /// The berth this ship moors at: the mooring of the object running the call.
            pub fn berth() -> u32 {
                Mooring::with(|mooring| mooring.berth)
            }

            /// Load `crates` more onto this ship, and answer how many it carries.
            pub fn load(crates: u32) -> u32 {
                Mooring::with(|mooring| {
                    mooring.cargo += crates;
                    mooring.cargo
                })
            }
        }

        /// The mooring of one ship: a resource of each `Ship` object, since it sits in the
        /// service's module. It depends on the harbor, so the kernel opens it after the harbor and
        /// closes it before, and both marked functions may use the harbor with `with`.
        #[resource(depends = [Harbor])]
        pub struct Mooring {
            pub berth: u32,
            pub cargo: u32,
        }

        impl Mooring {
            /// Take the next free berth; when every berth is taken, the ship is not created. The
            /// context here is the ship's: `object()` is its address.
            #[open]
            pub fn open() -> Result<Mooring, String> {
                let berth = Harbor::with(|harbor| {
                    if harbor.moored < harbor.berths {
                        harbor.moored += 1;
                        harbor.issued += 1;
                        Some(harbor.issued)
                    } else {
                        None
                    }
                });
                let Some(berth) = berth else {
                    return Err(String::from("every berth is taken"));
                };
                let ship = context::object().key.join("/");
                context::log(
                    context::Level::Info,
                    &format!("open mooring at berth {berth} for ship {ship}"),
                );
                Ok(Mooring { berth, cargo: 0 })
            }

            /// Give the berth back.
            #[close]
            pub fn close(self) {
                Harbor::with(|harbor| harbor.moored -= 1);
                context::log(
                    context::Level::Info,
                    &format!("close mooring at berth {}", self.berth),
                );
            }
        }
    }
}
