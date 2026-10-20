use tembra::prelude::*;
#[tembra::contract(local, name = "compile.missing-provider", version = 1)]
trait Counter { async fn increment(&self, by: u64) -> Result<u64>; }
struct Service;
impl CounterProvider for Service {}
