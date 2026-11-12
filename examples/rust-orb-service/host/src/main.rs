//! Install the artifact produced by `cargo tembra build` and call the Orb through Tembra.
//! The Orb knows nothing about this code, and this code knows nothing about the macro.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::Host;
use tembra::wasm::{Engine, Install, Limits};

/// The service Tembra derives from the WIT: `<orb>::<package>/<interface>@<version>`.
const SERVICE: &str = "greeter::example:greeter/greeting@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact = std::env::args_os()
        .nth(1)
        .ok_or("expected greeter.orb.wasm")?;
    // The artifact carries its contract. Its world imports nothing, so there is nothing to grant.
    let engine = Engine::new(Limits::default())?;
    let orb = engine.load(&std::fs::read(artifact)?, Install::default())?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb])?;
    let reference = Rc::new(host.get(SERVICE, vec!["main".into()], "alice")?);
    pool.run_until(reference.ready())?;
    let calls = reference.clone();
    let (hello, measured) = pool.run_until(host.root("alice", move |context| {
        Box::pin(async move {
            let hello = calls
                .call::<_, Value>(&context, "hello", json!(["Ada"]))
                .await?;
            let measured = calls
                .call::<_, Value>(&context, "measure", json!(["an orb written in Rust"]))
                .await?;
            Ok::<_, tembra::Error>((hello, measured))
        })
    }))?;
    println!("hello(\"Ada\") -> {hello}");
    println!("measure(\"an orb written in Rust\") -> {measured}");
    reference.release()?;
    pool.run_until(host.close())?;
    Ok(())
}
