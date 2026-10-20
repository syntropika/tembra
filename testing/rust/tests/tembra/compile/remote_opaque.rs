use tembra::prelude::*;
#[tembra::contract(remote, name = "compile.remote-opaque", version = 1, host = "service")]
trait Counter { #[opaque(id = "value@1")] fn value(&self) -> u64; }
