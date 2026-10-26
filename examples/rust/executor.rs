//! Run application work and Tembra callbacks on the same executor.

mod orb;

use futures::{FutureExt, executor::LocalPool, task::LocalSpawnExt};
use tembra::{Error, Host, Result};

/// Await a task result before calling a native service.
fn main() -> Result<()> {
    // 1. The application owns the executor; creating a host does not start a thread.
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    host.install(vec![orb::counter()])?;
    let counter = host.get(orb::COUNTER, vec!["main".into()], "host")?;

    // 2. Queue application work and keep its result handle.
    let input = executor
        .spawner()
        .spawn_local_with_handle(async {
            println!("Application task supplies 42");
            42_u32
        })
        .map_err(|error| Error::invalid("executor", error.to_string()))?;

    // 3. Await the result while the executor runs the task and service callbacks.
    let result = executor.run_until(host.root("host", move |context| {
        async move {
            let value = input.await;
            let count: u32 = counter.call(&context, orb::SET, value).await?;
            let read: u32 = counter.call(&context, orb::READ, ()).await?;
            assert_eq!(count, read);
            println!("Service stored {read}");
            Ok(())
        }
        .boxed_local()
    }));

    // 4. Run cleanup until shutdown has physically finished.
    let closed = executor.run_until(host.close());
    result?;
    closed
}
