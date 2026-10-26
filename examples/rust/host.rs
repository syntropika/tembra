//! Load an orb, call its service and await host cleanup.

mod orb;

use futures::{FutureExt, executor::LocalPool};
use tembra::{Host, Result};

/// Run the example on a local executor without a separate async runtime.
fn main() -> Result<()> {
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    host.install(vec![orb::counter()])?;
    let counter = host.get(orb::COUNTER, vec!["main".into()], "host")?;

    let result = executor.run_until(host.root("host", move |context| {
        async move {
            counter.call::<_, u32>(&context, orb::SET, 42_u32).await?;
            let count: u32 = counter.call(&context, orb::READ, ()).await?;
            println!("Count: {count}");
            Ok(())
        }
        .boxed_local()
    }));

    // Await cleanup even when a service call fails.
    let closed = executor.run_until(host.close());
    result?;
    closed
}
