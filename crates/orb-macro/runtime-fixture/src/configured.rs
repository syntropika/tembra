/// Notes, with what the crate's `archive` feature changes under `#[cfg]`s: a method of `Notes` for
/// each value of the feature, the service `Shelf` with its module, and the sub-module `archiving`
/// with the hook it declares. The macro expands the module once per value and the compiler keeps
/// the one being built, so the component, its WIT and the contract `cargo tembra build` writes from
/// it are those of the build's features, and so is `orb()`.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:notes@1.0.0", no_std)
)]
#[cfg_attr(not(feature = "no-std"), tembra::orb(package = "example:notes@1.0.0"))]
pub mod notebook {
    use core::sync::atomic::AtomicU32;

    /// How many notes are kept.
    static KEPT: AtomicU32 = AtomicU32::new(0);

    /// How many notes the shelf holds.
    #[cfg(feature = "archive")]
    static SHELVED: AtomicU32 = AtomicU32::new(0);

    pub mod notes {
        use super::KEPT;
        use alloc::string::String;
        use core::sync::atomic::Ordering;

        /// Keeps notes.
        #[service]
        pub struct Notes;

        impl Notes {
            /// Keep a note and return how many are kept.
            pub fn add(_text: String) -> u32 {
                KEPT.fetch_add(1, Ordering::Relaxed) + 1
            }

            /// Throw the notes away, without an archive to keep them, and return how many there
            /// were.
            #[cfg(not(feature = "archive"))]
            pub fn clear() -> u32 {
                KEPT.swap(0, Ordering::Relaxed)
            }

            /// Move the notes onto the shelf, tell the contributors to the archive's hook how many,
            /// and return how many there were.
            #[cfg(feature = "archive")]
            pub fn archive() -> u32 {
                let notes = KEPT.swap(0, Ordering::Relaxed);
                super::SHELVED.fetch_add(notes, Ordering::Relaxed);
                super::archiving::Archived::run(notes);
                notes
            }
        }
    }

    /// Holds the archived notes: the service's module goes with the `archive` feature.
    #[cfg(feature = "archive")]
    pub mod shelf {
        use super::SHELVED;
        use core::sync::atomic::Ordering;

        #[service]
        pub struct Shelf;

        impl Shelf {
            /// How many notes the shelf holds.
            pub fn count() -> u32 {
                SHELVED.load(Ordering::Relaxed)
            }
        }
    }

    /// The archive's hook: the sub-module and the hook it declares go with the `archive` feature.
    #[cfg(feature = "archive")]
    pub mod archiving {
        /// Every contributor learns how many notes each archive moved.
        #[hook(composition = series, input = u32)]
        pub struct Archived;
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::notebook;

    /// Without the `archive` feature, `orb()` registers `Notes` and its `clear`, and no shelf.
    #[cfg(not(feature = "archive"))]
    #[test]
    fn notes_without_the_archive_answer_natively() {
        use crate::native::{ORB, call, hosting, ready};
        use serde_json::json;

        let (mut pool, host) = hosting(notebook::orb().unwrap());
        let notes = ready(&mut pool, &host, "example:notes/notes@1.0.0");
        let mut answer = |method: &'static str, arguments| {
            call(&mut pool, &host, &notes, "alice", method, arguments)
        };
        assert_eq!(answer("add", json!(["one"])), json!(1));
        assert_eq!(answer("add", json!(["two"])), json!(2));
        assert_eq!(answer("clear", json!([])), json!(2));
        let shelf = format!("{ORB}::example:notes/shelf@1.0.0");
        assert!(host.get(&shelf, vec!["main".into()], "alice").is_err());
        notes.release().unwrap();
        pool.run_until(host.close()).unwrap();
    }

    /// With the `archive` feature, `orb()` holds the hook the feature adds, and answers why a
    /// native host cannot install it: only a component serves a hook's declaration.
    #[cfg(feature = "archive")]
    #[test]
    fn the_archive_adds_a_hook_only_a_component_declares() {
        assert_eq!(
            crate::native::refused(notebook::orb()),
            "the declaration of the hook example:notes/archived@1.0.0 works in a component build only: its `run` is synchronous, and natively a method has no fiber to suspend while the kernel runs the hook's contributions, so a native Orb declares no hook"
        );
    }
}
