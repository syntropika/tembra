/// Implements `example:counter/counter@1.0.0` from the contract the state example's build wrote,
/// whose `deps/` holds the kernel's package, `tembra-guest.wit`: with a state, a resource of the
/// root and the `context`, the module needs that package too, which the macro adds to its bindings
/// only when the consumed directory does not hold it already.
#[cfg_attr(
    feature = "no-std",
    tembra::orb(
        package = "example:steady@1.0.0",
        wit = "../../../examples/rust-orb-state/counter/contract",
        context,
        no_std
    )
)]
#[cfg_attr(
    not(feature = "no-std"),
    tembra::orb(
        package = "example:steady@1.0.0",
        wit = "../../../examples/rust-orb-state/counter/contract",
        context
    )
)]
pub mod steady {
    use alloc::{format, string::String};
    use core::cell::Cell;

    /// The count of one counter object, kept by the kernel: the consumed record.
    #[state]
    pub struct Kept {
        pub count: counter::Count,
    }

    impl Default for Kept {
        fn default() -> Self {
            Kept {
                count: counter::Count { total: 0, steps: 0 },
            }
        }
    }

    /// How many additions the Orb's counters made, a resource of the root.
    #[resource]
    pub struct Additions {
        pub made: Cell<u32>,
    }

    impl Additions {
        #[open]
        pub fn open() -> Additions {
            Additions { made: Cell::new(0) }
        }
    }

    /// The counter of the consumed contract.
    #[service("example:counter/counter@1.0.0")]
    pub struct Counter;

    impl Counter {
        pub fn add(by: i64) -> Result<counter::Count, String> {
            if context::principal() == "mallory" {
                return Err(format!("{} may not count", context::principal()));
            }
            Additions::with(|additions| additions.made.set(additions.made.get() + 1));
            let kept = Kept::update(|kept| {
                kept.count.total += by;
                kept.count.steps += 1;
            });
            Ok(kept.count)
        }

        pub fn read() -> counter::Count {
            Kept::read().count
        }
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::steady;
    use crate::native::{call, hosting, ready};
    use serde_json::{Value, json};

    /// Natively too, the macro reads the kernel's package once, from the consumed contract, and
    /// the Orb answers: each object counts in its state, through the root's resource, and the
    /// `context` names the call's principal, which the counter refuses for `mallory`.
    #[test]
    fn a_contract_holding_the_kernel_package_is_consumed_natively() {
        let (mut pool, host) = hosting(steady::orb().unwrap());
        let counter = ready(&mut pool, &host, "example:counter/counter@1.0.0");
        let mut add = |principal: &str, by: i64| {
            call(&mut pool, &host, &counter, principal, "add", json!([by]))
        };
        let ok =
            |total: i64, steps: u32| json!({"tag": "ok", "val": {"total": total, "steps": steps}});
        assert_eq!(add("alice", 2), ok(2, 1));
        assert_eq!(add("alice", 3), ok(5, 2));
        assert_eq!(
            add("mallory", 1),
            json!({"tag": "err", "val": "mallory may not count"})
        );
        assert_eq!(
            *counter.state::<Value>().unwrap().value().unwrap(),
            json!({"count": {"total": 5, "steps": 2}})
        );
        counter.release().unwrap();
        pool.run_until(host.close()).unwrap();
    }
}
