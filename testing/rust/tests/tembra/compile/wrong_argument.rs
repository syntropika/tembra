use tembra::prelude::*;
#[tembra::contract(local, name = "compile.wrong-argument", version = 1)]
trait Counter { async fn increment(&self, by: u64) -> Result<u64>; }
async fn wrong(counter: CounterRef) -> Result<()> {
    counter.increment("hello").await?;
    Ok(())
}
