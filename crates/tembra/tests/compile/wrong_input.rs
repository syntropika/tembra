use tembra::prelude::*;
#[tembra::contract(local, name = "compile.wrong-input", version = 1)]
trait Counter { async fn increment(&self, by: u64) -> Result<u64>; }
async fn wrong(host: &Host) -> Result<()> {
    host.get(Counter, "hello").await?;
    Ok(())
}
