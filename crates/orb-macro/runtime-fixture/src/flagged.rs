/// Implements `example:rights/rights@1.0.0` of the package in `rights/`, whose `access` is a WIT
/// `flags`, with the facade alone: its `wit-bindgen` has no `bitflags` feature, so the bindings
/// generate the type with `wit-bindgen`'s own `bitflags!`, whose constants, `empty` and operators
/// the module uses. A derived service takes and returns the same type.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(package = "example:holder@1.0.0", wit = "rights", no_std)
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(package = "example:holder@1.0.0", wit = "rights")
)]
pub mod holder {
    use alloc::vec::Vec;

    /// Every flag of the access.
    fn all() -> rights::Access {
        rights::Access::READ | rights::Access::WRITE | rights::Access::SHARE
    }

    /// The rights of the consumed package.
    #[service("example:rights/rights@1.0.0")]
    pub struct Rights;

    impl Rights {
        pub fn invert(access: rights::Access) -> rights::Access {
            access ^ all()
        }

        pub fn merged(grants: Vec<rights::Grant>) -> rights::Access {
            grants
                .iter()
                .fold(rights::Access::empty(), |merged, grant| {
                    merged | grant.access
                })
        }
    }

    /// Checks an access against what a holder wants.
    #[service]
    pub struct Checks;

    impl Checks {
        /// The flags of `wanted` that `access` lacks.
        pub fn missing(access: rights::Access, wanted: rights::Access) -> rights::Access {
            wanted & (access ^ all())
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::holder;
    use crate::native::{call, hosting, ready};
    use serde_json::json;

    /// Natively, the implemented interface and the derived service take and return the consumed
    /// flags as the component does: an object of its set flags in the boundary's JSON.
    #[test]
    fn a_consumed_flags_crosses_natively_with_the_facade_alone() {
        let (mut pool, host) = hosting(holder::orb().unwrap());
        let rights = ready(&mut pool, &host, "example:rights/rights@1.0.0");
        let checks = ready(&mut pool, &host, "example:holder/checks@1.0.0");
        let inverted = call(
            &mut pool,
            &host,
            &rights,
            "alice",
            "invert",
            json!([{"read": true}]),
        );
        assert_eq!(inverted, json!({"write": true, "share": true}));
        let wanted = json!([{"read": true}, {"read": true, "write": true}]);
        let missing = call(&mut pool, &host, &checks, "alice", "missing", wanted);
        assert_eq!(missing, json!({"write": true}));
        rights.release().unwrap();
        checks.release().unwrap();
        pool.run_until(host.close()).unwrap();
    }
}
