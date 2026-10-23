//! One object depends on another: a dashboard declares a counter dependency, re-exposes its
//! state, derives a value from it and calls it under the same call context.

use tembra::prelude::*;

// 1. The owner of the state.
#[tembra::contract(local, name = "example.counter", version = 1)]
trait Counter {
    #[state]
    fn count(&self) -> u32;
    async fn increment(&self) -> Result<u32>;
}

struct CounterService {
    count: Cell<u32>,
}

impl CounterProvider for CounterService {
    fn count(&self) -> Readable<u32> {
        self.count.readable()
    }

    async fn increment(&self, context: Context) -> Result<u32> {
        println!(
            "      [counter.increment] writing cell 'count' (root {})",
            context.core().root_id()
        );
        self.count.change(&context, |count| {
            *count += 1;
            Ok(())
        })?;
        self.count.value()
    }
}

// 2. The reader. It re-exposes the counter's state, derives from it and calls it.
#[tembra::contract(local, name = "example.dashboard", version = 1)]
trait Dashboard {
    /// The counter's `count`, re-exposed unchanged.
    #[state]
    fn mirrored(&self) -> u32;
    /// A value derived from the counter's state.
    #[state]
    fn doubled(&self) -> u64;
    /// Calls the counter under this call's own context.
    async fn bump(&self) -> Result<u32>;
}

struct DashboardService {
    // The dashboard's own claim on the counter, declared in its factory.
    counter: CounterRef,
    doubled: Readable<u64>,
}

impl DashboardProvider for DashboardService {
    fn mirrored(&self) -> Readable<u32> {
        // The readable of this claim's proxy port, not the counter's cell itself.
        self.counter.count()
    }

    fn doubled(&self) -> Readable<u64> {
        self.doubled.clone()
    }

    async fn bump(&self, context: Context) -> Result<u32> {
        println!(
            "      [dashboard.bump] calling counter.increment under the same context (root {})",
            context.core().root_id()
        );
        self.counter.with_context(&context).increment().await
    }
}

fn main() -> Result<()> {
    tembra::native::run("dependencies-example", (), |host| async move {
        let counter_facet = NativeFacet::new("counter", vec![], |_, setup| {
            println!("  [setup counter] preparing the 'count' state definition");
            let count = setup.state::<u32>("count")?;
            setup.serve(Counter, move |_, _, declaration| {
                println!("    [factory counter] creating cell 'count' = 0");
                Ok(CounterService {
                    count: declaration.state(count.value(0)?)?,
                })
            })
        })?;

        let dashboard_facet = NativeFacet::new("dashboard", vec![], |_, setup| {
            println!("  [setup dashboard] preparing the 'doubled' state definition");
            let doubled = setup.state::<u64>("doubled")?;
            setup.serve(Dashboard, move |_, _, declaration| {
                println!(
                    "    [factory dashboard] declaration.get(Counter): declaring the dependency"
                );
                let counter = declaration.get(Counter, CounterInput {})?;
                println!(
                    "    [factory dashboard] counter reference: status={:?} object_id={}",
                    counter.core().status(),
                    counter.core().object_id()
                );
                let doubled = declaration.derive(&doubled, (counter.count(),), |(count,)| {
                    Ok(u64::from(count) * 2)
                })?;
                Ok(DashboardService { counter, doubled })
            })
        })?;

        println!("=== 1. install both facets: setups run, factories do not ===");
        host.install(counter_facet, Default::default())?.await?;
        host.install(dashboard_facet, Default::default())?.await?;
        println!("    (no object exists yet)");

        println!(
            "=== 2. host.get(Dashboard): activates the dashboard, which activates the counter ==="
        );
        let dashboard = host.get(Dashboard, DashboardInput {}).await?;
        println!("    dashboard status = {:?}", dashboard.core().status());
        println!(
            "    dashboard.mirrored = {} (rev {})",
            dashboard.mirrored().value()?,
            dashboard.mirrored().snapshot()?.revision()
        );
        println!(
            "    dashboard.doubled  = {} (rev {})",
            dashboard.doubled().value()?,
            dashboard.doubled().snapshot()?.revision()
        );

        println!("=== 3. host.get(Counter): same address, same object, a second claim ===");
        let counter = host.get(Counter, CounterInput {}).await?;
        println!("    counter object_id = {}", counter.core().object_id());
        println!(
            "    counter.count = {} (rev {})",
            counter.count().value()?,
            counter.count().snapshot()?.revision()
        );

        println!("=== 4. subscribe to the dashboard's state ===");
        let mirrored = dashboard.mirrored().subscribe(|snapshot| {
            println!(
                "      >> mirrored = {} (rev {})",
                snapshot.value()?,
                snapshot.revision()
            );
            Ok(())
        })?;
        let doubled = dashboard.doubled().subscribe(|snapshot| {
            println!(
                "      >> doubled  = {} (rev {})",
                snapshot.value()?,
                snapshot.revision()
            );
            Ok(())
        })?;

        println!("=== 5. counter.increment(): write the counter directly ===");
        counter.increment().await?;
        println!(
            "    counter.count = {} (rev {})",
            counter.count().value()?,
            counter.count().snapshot()?.revision()
        );
        println!(
            "    dashboard.mirrored = {} (rev {})",
            dashboard.mirrored().value()?,
            dashboard.mirrored().snapshot()?.revision()
        );

        println!("=== 6. dashboard.bump(): reach the counter through the dashboard ===");
        dashboard.bump().await?;

        println!("=== 7. release the dashboard: the counter stays alive through `counter` ===");
        mirrored.unsubscribe();
        doubled.unsubscribe();
        let readable = dashboard.mirrored();
        drop(dashboard);
        println!(
            "    mirrored after the drop: {:?}",
            readable.value().map_err(|error| error.reason)
        );
        println!(
            "    counter.count still readable: {}",
            counter.count().value()?
        );

        println!("=== 8. release the counter: no claim remains, the object can be disposed ===");
        drop(counter);
        Ok(())
    })
}
