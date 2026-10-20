use tembra::prelude::*;
#[tembra::contract(local, name = "compile.wrong-provider", version = 1)]
trait Counter { async fn increment(&self, by: u64) -> Result<u64>; }
struct Service;
impl CounterProvider for Service {
    async fn increment(&self, by: String, _ctx: Context) -> Result<u64> { Ok(by.len() as u64) }
}
