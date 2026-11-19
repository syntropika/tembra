//! A compiled Orb whose service objects own resources. The harbor is a resource of the Orb's root,
//! opened once at activation; each `ship` object owns a mooring, opened when the kernel creates
//! the ship and closed when it goes. The mooring depends on the harbor: its `#[open]` function
//! takes the harbor's next berth, and its `#[close]` function gives it back. The harbor keeps a
//! log of what the moorings go through, which a method hands the host, so that the host sees the
//! kernel's order of opens and closes.
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra::orb(no_std, package = "example:harbor@1.0.0")]
pub mod port {
    use alloc::{string::String, vec::Vec};

    /// The harbor: a resource of the Orb's root, since it sits outside every service's module.
    #[resource]
    pub struct Harbor {
        /// How many berths there are.
        pub berths: u32,
        /// How many ships moor now.
        pub moored: u32,
        /// How many berths were issued so far, which numbers them.
        pub issued: u32,
        /// What the moorings went through since the host last asked, in order.
        pub log: Vec<String>,
    }

    impl Harbor {
        #[open]
        pub fn open() -> Harbor {
            Harbor {
                berths: 2,
                moored: 0,
                issued: 0,
                log: Vec::new(),
            }
        }
    }

    pub mod ship {
        use super::Harbor;
        use alloc::{format, string::String, vec::Vec};

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

            /// What the moorings of the harbor went through since the host last asked.
            pub fn log() -> Vec<String> {
                Harbor::with(|harbor| core::mem::take(&mut harbor.log))
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
            /// Take the next free berth; when every berth is taken, the ship is not created.
            #[open]
            pub fn open() -> Result<Mooring, String> {
                let berth = Harbor::with(|harbor| {
                    if harbor.moored < harbor.berths {
                        harbor.moored += 1;
                        harbor.issued += 1;
                        harbor
                            .log
                            .push(format!("open mooring at berth {}", harbor.issued));
                        Some(harbor.issued)
                    } else {
                        None
                    }
                });
                match berth {
                    Some(berth) => Ok(Mooring { berth, cargo: 0 }),
                    None => Err(String::from("every berth is taken")),
                }
            }

            /// Give the berth back.
            #[close]
            pub fn close(self) {
                Harbor::with(|harbor| {
                    harbor.moored -= 1;
                    harbor
                        .log
                        .push(format!("close mooring at berth {}", self.berth));
                });
            }
        }
    }
}
