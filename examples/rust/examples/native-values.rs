//! Pass an ordinary Rust value through a typed local method without serialization.

use std::{cell::Cell as NativeCell, rc::Rc};
use tembra::prelude::*;

// 1. Local method arguments can retain their native Rust types and identity.
#[tembra::contract(local, name = "example.native-counter", version = 1)]
trait NativeCounter {
    async fn increment(&self, value: Rc<NativeCell<u32>>) -> Result<Rc<NativeCell<u32>>>;
}

struct CounterService;
impl NativeCounterProvider for CounterService {
    async fn increment(
        &self,
        value: Rc<NativeCell<u32>>,
        _context: Context,
    ) -> Result<Rc<NativeCell<u32>>> {
        let next = value.get().checked_add(1).ok_or_else(|| {
            RuntimeError::invalid("counter_overflow", "counter reached its maximum value")
        })?;
        value.set(next);
        Ok(value)
    }
}

fn main() -> Result<()> {
    tembra::native::run("native-values-example", (), |host| async move {
        // 2. Register a synchronous provider and acquire its ready typed reference.
        host.serve(NativeCounter, |_, _, _| Ok(CounterService))?;
        let counter = host.get(NativeCounter, NativeCounterInput {}).await?;

        // 3. The returned Rc points to the same value; no JSON conversion is involved.
        let value = Rc::new(NativeCell::new(0));
        let returned = counter.increment(value.clone()).await?;
        println!(
            "Same native value: {}; count: {}",
            Rc::ptr_eq(&value, &returned),
            value.get()
        );

        // 4. Release the reference; the runner awaits Host cleanup after the callback.
        drop(counter);
        Ok(())
    })
}
