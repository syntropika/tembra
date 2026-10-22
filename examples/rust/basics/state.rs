//! Observe managed state, a derived value and immutable snapshots.

use tembra::prelude::*;

// 1. Expose readable state and one method that changes it.
#[tembra::contract(local, name = "example.state", version = 1)]
trait Counter {
    #[state]
    fn count(&self) -> u32;
    #[state]
    fn double(&self) -> u64;
    async fn add(&self, by: u32) -> Result<u32>;
}

struct CounterService {
    count: Cell<u32>,
    double: Readable<u64>,
}
impl CounterProvider for CounterService {
    fn count(&self) -> Readable<u32> {
        self.count.readable()
    }
    fn double(&self) -> Readable<u64> {
        self.double.clone()
    }
    async fn add(&self, by: u32, context: Context) -> Result<u32> {
        self.count.change(&context, |count| {
            *count = count.checked_add(by).ok_or_else(|| {
                RuntimeError::invalid("counter_overflow", "counter reached its maximum value")
            })?;
            Ok(())
        })?;
        self.count.value()
    }
}

fn main() -> Result<()> {
    tembra::native::run("state-example", (), |host| async move {
        // 2. Declare the cell and its derived value together in the provider factory.
        host.install(
            NativeFacet::new("counter-state", vec![], |_, setup| {
                let count_definition = setup.state::<u32>("count")?;
                let double_definition = setup.state::<u64>("double")?;
                setup.serve(Counter, move |_, _, declaration| {
                    let count = declaration.state(count_definition.value(0_u32)?)?;
                    let double = declaration.derive(
                        &double_definition,
                        (count.readable(),),
                        |(count,)| Ok(u64::from(count) * 2),
                    )?;
                    Ok(CounterService { count, double })
                })?;
                Ok(())
            })?,
            Default::default(),
        )?
        .await?;
        let counter = host.get(Counter, CounterInput {}).await?;

        // 3. Keep the reference alive while observing its weak readable views.
        let initial = counter.count().snapshot()?;
        let double = counter.double();
        let subscription = counter.count().subscribe(move |snapshot| {
            println!("count={}, double={}", snapshot.value()?, double.value()?);
            Ok(())
        })?;

        // 4. Mutations publish updated state; an earlier snapshot keeps its original value.
        counter.add(3).await?;
        counter.add(2).await?;
        println!("Initial snapshot still contains {}", initial.value()?);

        // 5. Unsubscribe before releasing the claim. The runner then joins Host cleanup.
        subscription.unsubscribe();
        drop(counter);
        Ok(())
    })
}
