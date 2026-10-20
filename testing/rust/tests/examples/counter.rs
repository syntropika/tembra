//! Exercise the actual introductory provider rather than copying its implementation.

#[path = "../../../../examples/rust/src/counter.rs"]
mod counter;

use counter::{Counter, CounterInput, CounterService};
use tembra::prelude::*;

#[test]
fn example_counter_exposes_its_committed_increment() -> Result<()> {
    tembra::native::run("example-counter-behavior", (), |host| async move {
        host.serve(Counter, |_, _, declaration| {
            CounterService::new(declaration, 0)
        })?;
        let counter = host.get(Counter, CounterInput {}).await?;
        assert_eq!(counter.count().value()?, 0);
        assert_eq!(counter.increment().await?, 1);
        assert_eq!(counter.count().value()?, 1);
        drop(counter);
        Ok(())
    })
}

#[test]
fn example_counter_refuses_overflow_without_mutating_state() -> Result<()> {
    tembra::native::run("example-counter-limit", (), |host| async move {
        host.serve(Counter, |_, _, declaration| {
            CounterService::new(declaration, u32::MAX)
        })?;
        let counter = host.get(Counter, CounterInput {}).await?;
        let error = counter.increment().await.expect_err("overflow must fail");
        assert_eq!(error.code, tembra::ErrorCode::Invalid);
        assert_eq!(error.reason, "counter_overflow");
        assert_eq!(counter.count().value()?, u32::MAX);
        drop(counter);
        Ok(())
    })
}
