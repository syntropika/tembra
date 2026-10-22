//! Start here: register a typed counter, call it, then finish managed cleanup.

mod counter;

use counter::{Counter, CounterInput, CounterService};
use tembra::prelude::*;

fn main() -> Result<()> {
    // 1. Run one Host. The runner awaits its shutdown after this callback returns.
    tembra::native::run("counter-example", (), |host| async move {
        // 2. Register the provider. Its synchronous factory declares the initial state.
        host.install(
            NativeFacet::new("counter", vec![], |_, setup| {
                let count = setup.state::<u32>("count")?;
                setup.serve(Counter, move |_, _, declaration| {
                    CounterService::new(declaration, count.value(0)?)
                })
            })?,
            Default::default(),
        )?
        .await?;

        // 3. Acquire a ready reference, then invoke the typed method.
        let counter = host.get(Counter, CounterInput {}).await?;
        counter.increment().await?;
        println!("Count: {}", counter.count().value()?);

        // 4. Drop the claim when done. The runner joins asynchronous cleanup on exit.
        drop(counter);
        Ok(())
    })
}
