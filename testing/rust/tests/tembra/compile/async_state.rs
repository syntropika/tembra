use tembra::prelude::*;
#[tembra::contract(local, name = "compile.async-state", version = 1)]
trait Counter { #[state] async fn count(&self) -> u64; }
