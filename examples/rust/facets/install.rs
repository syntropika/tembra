//! Group typed provider registrations into a native facet.

#[path = "../src/counter.rs"]
mod counter;

use counter::{Counter, CounterInput, CounterService};
use tembra::prelude::*;

fn main() -> Result<()> {
    tembra::native::run("facet-example", (), |host| async move {
        // 1. Prepare registrations. Setup runs when this native facet is installed.
        let facet = NativeFacet::new("counter", vec![], |_scope, declarations| {
            let count = declarations.state::<u32>("count")?;
            declarations.serve(Counter, move |_, _, declaration| {
                CounterService::new(declaration, count.value(0)?)
            })
        })?;

        // 2. Install the facet and await its startup before using its services.
        host.install(facet, Default::default())?.await?;

        // 3. Use exactly the same typed reference as a directly registered provider.
        let counter = host.get(Counter, CounterInput {}).await?;
        println!("Count from the facet: {}", counter.increment().await?);

        // 4. Release the claim. The runner owns and awaits final Host cleanup.
        drop(counter);
        Ok(())
    })
}
