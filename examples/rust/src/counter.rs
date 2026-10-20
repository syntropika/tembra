//! The local counter contract and its private state provider.

use tembra::prelude::*;

#[tembra::contract(local, name = "example.counter", version = 1)]
pub(crate) trait Counter {
    /// Observes the managed count, initially zero.
    #[state]
    fn count(&self) -> u32;

    /// Returns the current count or the core's scope/state error.
    async fn read(&self) -> Result<u32>;

    /// Adds one and returns the new count.
    ///
    /// # Errors
    /// Returns `invalid/counter_overflow` at `u32::MAX` without changing state,
    /// or the core's scope/state error.
    async fn increment(&self) -> Result<u32>;
}

struct CounterService {
    count: Cell<u32>,
}

impl CounterProvider for CounterService {
    fn count(&self) -> Readable<u32> {
        self.count.readable()
    }

    async fn read(&self, _context: Context) -> Result<u32> {
        self.count.value()
    }

    async fn increment(&self, context: Context) -> Result<u32> {
        self.count.change(&context, |count| {
            let updated = count.checked_add(1).ok_or_else(|| {
                RuntimeError::invalid("counter_overflow", "counter reached its maximum value")
            })?;
            *count = updated;
            Ok(())
        })?;
        self.count.value()
    }
}

pub(crate) fn serve(host: &Host) -> Result<()> {
    host.serve(Counter, |_, _, declaration| {
        Ok(CounterService {
            count: declaration.state("count", 0_u32)?,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_refuses_mutation_through_the_typed_reference() -> Result<()> {
        tembra::native::run("counter-limit", (), |host| async move {
            host.serve(Counter, |_, _, declaration| {
                Ok(CounterService {
                    count: declaration.state("count", u32::MAX)?,
                })
            })?;
            let counter = host.get(Counter, CounterInput {}).await?;
            let error = counter.increment().await.expect_err("overflow must fail");
            assert_eq!(error.code, tembra::ErrorCode::Invalid);
            assert_eq!(error.reason, "counter_overflow");
            assert_eq!(counter.read().await?, u32::MAX);
            assert_eq!(counter.count().value()?, u32::MAX);
            counter.core().release();
            Ok(())
        })
    }
}
