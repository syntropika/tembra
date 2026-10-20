use tembra::prelude::*;
#[tembra::contract(local, name = "compile.reserved", version = 1)]
trait Counter { async fn with_context(&self) -> Result<u64>; }
