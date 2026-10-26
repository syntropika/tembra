//! Compare shared and independent objects from one service.

mod orb;

use futures::{FutureExt, executor::LocalPool};
use tembra::{Host, Result};

/// Show how service, key and principal define an object address.
fn main() -> Result<()> {
    // 1. Install one recipe for every counter address.
    let mut executor = LocalPool::new();
    let host = Host::new(executor.spawner());
    host.install(vec![orb::counter()])?;

    // 2. Equal addresses share an object; a different key or principal does not.
    let visits = host.get(orb::COUNTER, vec!["visits".into()], "host")?;
    let again = host.get(orb::COUNTER, vec!["visits".into()], "host")?;
    let errors = host.get(orb::COUNTER, vec!["errors".into()], "host")?;
    let alice = host.get(orb::COUNTER, vec!["visits".into()], "alice")?;
    assert_eq!(visits.object(), again.object());
    assert_ne!(visits.object(), errors.object());
    assert_ne!(visits.object(), alice.object());
    println!("Same address shares an object; another key or principal creates another.");

    // 3. A change is visible through another claim on the same object.
    let result = executor.run_until(host.root("host", move |context| {
        async move {
            visits.call::<_, u32>(&context, orb::SET, 42_u32).await?;
            let shared: u32 = again.call(&context, orb::READ, ()).await?;
            let separate: u32 = errors.call(&context, orb::READ, ()).await?;
            assert_eq!((shared, separate), (42, 0));
            println!("Visits: {shared}; errors: {separate}");
            Ok(())
        }
        .boxed_local()
    }));

    // 4. Release the remaining claim and finish physical cleanup.
    drop(alice);
    let closed = executor.run_until(host.close());
    result?;
    closed
}
