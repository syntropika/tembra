use tembra::prelude::*;
#[tembra::contract(local, name = "compile.generic", version = 1)]
trait Counter<T> { async fn echo(&self, value: T) -> Result<T>; }
