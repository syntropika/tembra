use alloc::string::String;

/// Note what a resource went through. A component writes it to the host's log, since its `context`
/// serves a resource's functions; natively the note goes to the journal the native tests read.
fn note(event: String) {
    #[cfg(target_arch = "wasm32")]
    cellar::context::log(cellar::context::Level::Info, &event);
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

/// Two resources of the Orb's root, the cache depending on the database: the kernel opens the
/// database first, whose rows the cache's `#[open]` function reads, and closes the cache first,
/// whose `#[close]` function writes the rows it took back to the database.
/// The cache comes first in the module, so only its dependency puts it after the database.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:cellar@1.0.0", context, no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:cellar@1.0.0", context)
)]
pub mod cellar {
    use alloc::format;

    /// The rows the database held when the cache opened.
    #[resource(depends = [Db])]
    pub struct Cache {
        pub rows: u32,
    }

    impl Cache {
        #[open]
        pub fn open() -> Cache {
            let rows = Db::with(|db| db.rows);
            super::note(format!("open cache, {rows} rows"));
            Cache { rows }
        }

        #[close]
        pub fn close(self) {
            let rows = Db::with(|db| {
                db.rows += self.rows;
                db.rows
            });
            super::note(format!("close cache, {rows} rows"));
        }
    }

    /// The database, and its rows.
    #[resource]
    pub struct Db {
        pub rows: u32,
    }

    impl Db {
        #[open]
        pub fn open() -> Db {
            super::note("open db".into());
            Db { rows: 3 }
        }

        #[close]
        pub fn close(self) {
            super::note(format!("close db, {} rows", self.rows));
        }
    }

    pub mod shelf {
        use super::Cache;

        /// Reads the cache.
        #[service]
        pub struct Shelf;

        impl Shelf {
            /// The rows the cache holds.
            pub fn rows() -> u32 {
                Cache::with(|cache| cache.rows)
            }
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::{JOURNAL, cellar};
    use crate::native::{call, hosting, ready};
    use serde_json::json;

    /// What the resources went through since the last call.
    fn journal() -> Vec<String> {
        JOURNAL.with(|journal| journal.take())
    }

    /// Natively, the host opens the database before the cache, whose `#[open]` function reads the
    /// database, and closes the cache, whose `#[close]` function writes to the database, before
    /// the database.
    #[test]
    fn a_resource_opens_after_and_closes_before_its_dependency_natively() {
        let (mut pool, host) = hosting(cellar::orb().unwrap());
        let shelf = ready(&mut pool, &host, "example:cellar/shelf@1.0.0");
        assert_eq!(
            call(&mut pool, &host, &shelf, "alice", "rows", json!([])),
            json!(3)
        );
        assert_eq!(journal(), ["open db", "open cache, 3 rows"]);
        shelf.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(journal(), ["close cache, 6 rows", "close db, 6 rows"]);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
    }
}
