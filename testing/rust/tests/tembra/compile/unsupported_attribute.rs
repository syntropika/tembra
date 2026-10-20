use tembra::prelude::*;
#[tembra::contract(local, name = "compile.attribute", version = 1)]
trait Counter { #[native] async fn echo(&self, value: u64) -> Result<u64>; }
