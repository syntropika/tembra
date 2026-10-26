//! Load an orb, call its service and await host cleanup.

mod orb;

use futures::{FutureExt, executor::LocalPool};
use tembra::{Host, Result};

/// Run the example on a local executor without a separate async runtime.
fn main() -> Result<()> {
    // 1. Supply the executor that runs calls and cleanup.
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());

    // 2. Install the orb before acquiring its service.
    host.install(vec![orb::counter()])?;
    let counter = host.get(orb::COUNTER, vec!["main".into()], "host")?;

    // 3. Keep both calls inside one operation context.
    let result = executor.run_until(host.root("host", move |context| {
        async move {
            // 4. Validate the proposed state before publishing it.
            counter.call::<_, u32>(&context, orb::SET, 42_u32).await?;
            let count: u32 = counter.call(&context, orb::READ, ()).await?;
            println!("Count: {count}");
            // The reference is released when this operation returns.
            Ok(())
        }
        .boxed_local()
    }));

    // 5. Await cleanup even when a service call fails.
    let closed = executor.run_until(host.close());
    result?;
    closed
}
