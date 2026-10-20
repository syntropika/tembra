#[tembra::contract(local,name="compile.non-provider",version=1)]
trait Counter { async fn read(&self)->tembra::Result<u64>; }
fn wrong(host:&tembra::Host) { host.serve(Counter, |_,_,_| Ok(42_u64)); }
