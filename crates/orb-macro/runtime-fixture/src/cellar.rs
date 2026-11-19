use alloc::string::String;

/// Note what a resource went through, in the host's log: the module's `context` serves a
/// resource's functions, as a component and natively, where the host reads the records through
/// `Host::on_log`.
fn note(event: String) {
    cellar::context::log(cellar::context::Level::Info, &event);
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
            // The context of a root resource's opener: the Orb's root, as a hook contribution's.
            let address = context::object();
            super::note(format!(
                "open db at [{}] of {}",
                address.key.join("/"),
                address.principal
            ));
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
    use super::cellar;
    use crate::native::{call, hosting, logged, ready};
    use serde_json::json;

    /// Natively, the host opens the database before the cache, whose `#[open]` function reads the
    /// database, and closes the cache, whose `#[close]` function writes to the database, before
    /// the database; the `#[open]` and `#[close]` functions log through the module's `context`,
    /// as a component's do, with the root's address.
    #[test]
    fn a_resource_opens_after_and_closes_before_its_dependency_natively() {
        let (mut pool, host) = hosting(cellar::orb().unwrap());
        let logged = logged(&host);
        let shelf = ready(&mut pool, &host, "example:cellar/shelf@1.0.0");
        assert_eq!(
            call(&mut pool, &host, &shelf, "alice", "rows", json!([])),
            json!(3)
        );
        assert_eq!(
            logged(),
            ["open db at [] of host", "open cache, 3 rows"]
        );
        shelf.release().unwrap();
        pool.run_until(host.close()).unwrap();
        assert_eq!(logged(), ["close cache, 6 rows", "close db, 6 rows"]);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
    }
}
