//! A typed counter: callers observe state and invoke a checked mutation.

use tembra::prelude::*;

// 1. Declare the interface. The macro generates the input, provider trait and reference.
#[tembra::contract(local, name = "example.counter", version = 1)]
pub(crate) trait Counter {
    #[state]
    fn count(&self) -> u32;

    /// Adds one, refusing overflow without changing the count.
    async fn increment(&self) -> Result<u32>;
}

// 2. Keep writable state private; consumers receive a Readable instead of the Cell.
pub(crate) struct CounterService {
    count: Cell<u32>,
}

impl CounterService {
    pub(crate) fn new(declaration: &mut Declaration<'_>, initial: u32) -> Result<Self> {
        Ok(Self {
            count: declaration.state("count", initial)?,
        })
    }
}

// 3. Mutate through the call's Context. A failed change leaves committed state untouched.
impl CounterProvider for CounterService {
    fn count(&self) -> Readable<u32> {
        self.count.readable()
    }

    async fn increment(&self, context: Context) -> Result<u32> {
        self.count.change(&context, |count| {
            *count = count.checked_add(1).ok_or_else(|| {
                RuntimeError::invalid("counter_overflow", "counter reached its maximum value")
            })?;
            Ok(())
        })?;
        self.count.value()
    }
}
