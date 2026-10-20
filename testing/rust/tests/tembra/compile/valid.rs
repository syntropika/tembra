use tembra::prelude::*;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(crate = "tembra::serde")]
#[schemars(crate = "tembra::schemars")]
struct Input { name: String }

#[tembra::contract(local, name = "compile.counter", version = 1, input = Input)]
trait Counter {
    async fn increment(&self, by: u64) -> Result<u64>;
}

struct Service;
impl CounterProvider for Service {
    async fn increment(&self, by: u64, _ctx: Context) -> Result<u64> { Ok(by) }
}

async fn use_counter(host: &Host) -> Result<()> {
    host.serve(Counter, |_, _, _| Ok(Service))?;
    let counter = host.get(Counter, Input { name: "visits".into() }).await?;
    let _: u64 = counter.increment(2).await?;
    Ok(())
}

struct P;
#[tembra::contract(local, name = "compile.names", version = 1)]
trait Names {
    async fn r#match(&self, provider: P) -> Result<P>;
}
impl NamesProvider for Service {
    async fn r#match(&self, provider: P, _ctx: Context) -> Result<P> { Ok(provider) }
}

#[tembra::contract(local, name = "compile.empty", version = 1)]
trait Empty {}
impl EmptyProvider for Service {}

struct __TembraProvider0;
#[tembra::contract(local, name="compile.generic-name", version=1)]
trait GenericName { async fn echo(&self, args: r#__TembraProvider0) -> Result<r#__TembraProvider0>; }
impl GenericNameProvider for Service {
    async fn echo(&self, args: __TembraProvider0, _ctx: Context)->Result<__TembraProvider0> { Ok(args) }
}
