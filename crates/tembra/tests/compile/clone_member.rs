#[tembra::contract(local,name="compile.clone",version=1)]
trait Counter { async fn clone(&self)->tembra::Result<u64>; }
