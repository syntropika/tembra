#[tembra::contract(local,name="compile.async-factory",version=1)]
trait Counter { async fn read(&self)->tembra::Result<u64>; }
struct Service;
impl CounterProvider for Service {
    async fn read(&self,_ctx:tembra::Context)->tembra::Result<u64> { Ok(0) }
}
fn wrong(host:&tembra::Host) {
    host.serve(Counter, |_,_,_| async { Ok(Service) });
}
