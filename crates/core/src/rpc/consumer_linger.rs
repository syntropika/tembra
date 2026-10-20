use crate::{Result, RuntimeError};
use std::{collections::BTreeMap, time::Duration};

/// Checked, immutable optional retention of idle consumer bindings, by contract name.
///
/// The default map is empty (no retention) and the idle capacity is four. This policy
/// never retains a released Reference or changes provider-side object linger.
#[derive(Clone, Debug)]
pub struct ConsumerLingerOptions {
    types: BTreeMap<String, Duration>,
    capacity: usize,
}

impl Default for ConsumerLingerOptions {
    fn default() -> Self {
        Self {
            types: BTreeMap::new(),
            capacity: 4,
        }
    }
}

impl ConsumerLingerOptions {
    /// Checks the entire policy without creating bindings, timers or provider effects.
    /// Zero capacity or a zero TTL disables the corresponding retention.
    ///
    /// # Errors
    /// Refuses more than 1,024 types, capacity above 1,024, empty names, fractional
    /// milliseconds, and millisecond durations above the portable safe-integer bound.
    ///
    /// ```
    /// use std::{collections::BTreeMap, time::Duration};
    /// use tembra_core::rpc::ConsumerLingerOptions;
    /// let options = ConsumerLingerOptions::new(
    ///     BTreeMap::from([("counter".into(), Duration::from_millis(40))]), 2)?;
    /// assert_eq!(options.ttl("counter"), Duration::from_millis(40));
    /// assert_eq!(options.ttl("other"), Duration::ZERO);
    /// assert_eq!(options.capacity(), 2);
    /// let invalid = ConsumerLingerOptions::new(
    ///     BTreeMap::from([("counter".into(), Duration::from_nanos(1))]), 2).unwrap_err();
    /// assert_eq!(invalid.reason, "consumer_linger");
    /// assert_eq!(invalid.details()["field"], "ttl");
    /// assert_eq!(ConsumerLingerOptions::default().ttl("counter"), Duration::ZERO);
    /// # Ok::<(), tembra_core::RuntimeError>(())
    /// ```
    pub fn new(types: BTreeMap<String, Duration>, capacity: usize) -> Result<Self> {
        if capacity > 1024 {
            return Err(invalid("capacity", serde_json::json!(capacity)));
        }
        if types.len() > 1024 {
            return Err(invalid("types", serde_json::json!(types.len())));
        }
        for (name, ttl) in &types {
            if name.is_empty() {
                return Err(invalid("name", serde_json::json!(name)));
            }
            if ttl.subsec_nanos() % 1_000_000 != 0 || ttl.as_millis() > 9_007_199_254_740_991 {
                return Err(invalid("ttl", serde_json::json!({"name":name})));
            }
        }
        Ok(Self { types, capacity })
    }

    /// Returns the configured duration, or zero for an unlisted contract name.
    /// Zero disables that type; a zero capacity disables retention even for a nonzero duration.
    pub fn ttl(&self, contract_name: &str) -> Duration {
        self.types.get(contract_name).copied().unwrap_or_default()
    }

    /// Returns the Host-wide maximum number of idle consumer bindings, including all types.
    /// Active claims are not idle entries. Claimed demand can evict idle entries to satisfy
    /// its own outgoing connection hold limit. Provider hold and stream budgets are not
    /// reclaimed automatically and can refuse new holds.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

fn invalid(field: &str, value: serde_json::Value) -> RuntimeError {
    RuntimeError::invalid("consumer_linger", "invalid consumer linger policy")
        .with_details(serde_json::json!({"field":field,"value":value}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{ConnectionOptions, MemoryLinkOptions, RootOrigin, memory_link};
    use crate::{GetOptions, Host, HostOptions, Implementation, ManualDriver, ServeOptions};
    use std::rc::Rc;
    use tembra_contracts::{Contract, Placement};

    #[test]
    fn idle_timer_is_not_a_strong_host_or_claim_owner() {
        let driver = ManualDriver::new();
        let policy = ConsumerLingerOptions::new(
            BTreeMap::from([("weak".into(), Duration::from_secs(10))]),
            1,
        )
        .unwrap();
        let client = Host::new(
            HostOptions::new("client", serde_json::json!(null)).with_consumer_linger(policy),
            driver.clone(),
        );
        let server = Host::new(
            HostOptions::new("server", serde_json::json!(null)),
            driver.clone(),
        );
        server
            .set_rpc_identity(
                RootOrigin::new("server", "11111111111111111111111111111111").unwrap(),
            )
            .unwrap();
        let token = Rc::new(
            Contract::remote(
                "weak",
                1,
                serde_json::json!({}),
                Placement::Named {
                    name: "server".into(),
                },
                BTreeMap::from([("default".into(), BTreeMap::new())]),
            )
            .unwrap(),
        );
        server
            .serve(
                token.clone(),
                Rc::new(|_, _, _| Ok(Implementation::new().variant("default"))),
                ServeOptions::default(),
            )
            .unwrap();
        let (left, right, _) = memory_link(driver.clone(), MemoryLinkOptions::default()).unwrap();
        let outgoing = client
            .connect_client(left, "server", ConnectionOptions::default())
            .unwrap();
        let incoming = server
            .accept_client(right, serde_json::json!(null), ConnectionOptions::default())
            .unwrap();
        client.set_route("server", outgoing.clone()).unwrap();
        let reference = driver
            .run_until(client.get(token, serde_json::json!({}), GetOptions::default()))
            .unwrap();
        reference.release();
        drop(reference);
        driver.run_until_stalled();
        let weak = Rc::downgrade(&client.inner);
        drop(outgoing);
        drop(client);
        assert!(
            weak.upgrade().is_none(),
            "a pending idle timer must not retain the Host"
        );
        driver.advance(Duration::from_secs(10));
        driver.run_until_stalled();
        incoming.close(crate::rpc::CloseReason::closed());
        driver.run_until(server.close()).unwrap();
    }
}
