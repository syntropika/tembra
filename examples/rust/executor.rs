//! Drive application work and Tembra callbacks with the same executor.

mod orb;

use futures::{FutureExt, channel::oneshot, executor::LocalPool, task::LocalSpawnExt};
use tembra::{Error, Host, Result};

/// Resume a service call when another local task supplies its input.
fn main() -> Result<()> {
    // 1. The application owns the executor; creating a host does not start a thread.
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    host.install(vec![orb::counter()])?;
    let counter = host.get(orb::COUNTER, vec!["main".into()], "host")?;

    // 2. Queue ordinary application work on that same executor.
    let (send, receive) = oneshot::channel();
    executor
        .spawner()
        .spawn_local(async move {
            println!("Application task supplies 42");
            let _ = send.send(42_u32);
        })
        .map_err(|error| Error::invalid("executor", error.to_string()))?;

    // 3. Driving the root also drives the queued task and native callbacks.
    let result = executor.run_until(host.root("host", move |context| {
        async move {
            let value = receive
                .await
                .map_err(|error| Error::invalid("input", error.to_string()))?;
            let count: u32 = counter.call(&context, orb::SET, value).await?;
            let read: u32 = counter.call(&context, orb::READ, ()).await?;
            assert_eq!(count, read);
            println!("Service stored {read}");
            Ok(())
        }
        .boxed_local()
    }));

    // 4. Keep driving the executor until shutdown has physically finished.
    let closed = executor.run_until(host.close());
    result?;
    closed
}
