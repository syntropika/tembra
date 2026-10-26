//! Retain an idle resource, then expire it with a host-owned clock.

use futures::{FutureExt, executor::LocalPool};
use tembra::{Host, Orb, Result, Service, core::Lifetime};

const CONNECTION: &str = "example.connection";

/// Show reuse during linger and reconstruction after expiry.
fn main() -> Result<()> {
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());

    // 1. Declare a managed resource and a 100 ms idle retention period.
    let service = Service::new(CONNECTION, |declaration| {
        declaration.resource(
            "connection",
            1,
            || {
                println!("Open connection");
                Ok(())
            },
            |_, _| {
                async {
                    println!("Close connection");
                    Ok(())
                }
                .boxed_local()
            },
        )
    })
    .lifetime(Lifetime::Referenced, 100);
    host.install(vec![Orb::new(CONNECTION, 1).service(service)])?;

    // 2. Activation runs the opener before the reference becomes ready.
    let first = host.get(CONNECTION, vec!["main".into()], "host")?;
    executor.run_until(first.ready())?;
    let original = first.object();
    drop(first);

    // 3. Reacquiring during linger keeps the same resource and object.
    let second = host.get(CONNECTION, vec!["main".into()], "host")?;
    assert_eq!(second.object(), original);
    println!("Reused idle connection");
    drop(second);

    // 4. Supply monotonic elapsed time; no real sleep is needed here.
    host.kernel().tick(100)?;
    host.flush();
    executor.run_until_stalled();

    // 5. After cleanup, the same address creates a fresh resource.
    let third = host.get(CONNECTION, vec!["main".into()], "host")?;
    executor.run_until(third.ready())?;
    assert_ne!(third.object(), original);
    println!("Created a new connection after expiry");
    drop(third);
    executor.run_until(host.close())
}
