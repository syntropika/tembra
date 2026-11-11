//! The wiring: install the compiled `greeter` and the compiled `welcomer`, which imports the
//! greeter's interface, and call the welcomer. The installer binds the import: the welcomer names
//! an interface, the host names the Orb that provides it. No Orb is defined here.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::Host;
use tembra_executor_wasm::{Engine, Install, Limits};

/// The greeter's interface, imported by the welcomer and bound to the `greeter` Orb at installation.
const GREETING: &str = "example:greeter/greeting@1.0.0";
const WELCOMER: &str = "welcomer::example:welcomer/welcoming@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let (Some(greeter), Some(welcomer)) = (arguments.next(), arguments.next()) else {
        return Err("expected greeter.orb.wasm and welcomer.orb.wasm".into());
    };
    let engine = Engine::new(Limits::default())?;
    let greeter = std::fs::read(greeter)?;
    let welcomer = std::fs::read(welcomer)?;

    // 1. The welcomer's world imports an interface. Loading it without naming a provider is
    //    refused: binding an import is the installer's job, not the Orb's.
    match engine.load(&welcomer, Install::default()) {
        Ok(_) => println!("load without requires -> accepted"),
        Err(error) => println!("load without requires -> refused: {error}"),
    }

    // 2. `requires` binds the import to the Orb named `greeter`, API version 1. The kernel checks
    //    the greeter's contract against the signature the welcomer was built with.
    let greeter = engine.load(&greeter, Install::default())?;
    let welcomer = engine.load(
        &welcomer,
        Install::default().requires(GREETING, "greeter", 1),
    )?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![greeter, welcomer])?;

    // 3. One call to the welcomer becomes two calls to the greeter, under the same principal.
    let reference = Rc::new(host.get(WELCOMER, vec!["main".into()], "alice")?);
    pool.run_until(reference.ready())?;
    let calls = reference.clone();
    let welcomed = pool.run_until(host.root("alice", move |context| {
        Box::pin(async move {
            calls
                .call::<_, Value>(&context, "welcome", json!([["Ada", "Grace"]]))
                .await
        })
    }))?;
    println!("welcome([\"Ada\", \"Grace\"]) -> {welcomed}");
    reference.release()?;
    pool.run_until(host.close())?;
    Ok(())
}
