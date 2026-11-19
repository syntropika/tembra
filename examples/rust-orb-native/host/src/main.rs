//! A native host linking two Orb crates that implement the same interface. Each crate is an
//! ordinary Cargo dependency, and its module's `orb()` is the Orb the host installs: no component
//! and no WebAssembly engine, and the compiler checks the calls made through the typed clients.
use futures::executor::LocalPool;
use std::rc::Rc;
use tembra::Host;
use tembra_example_english::english;
use tembra_example_italian::italian;

/// The interface both Orbs implement, from the shared WIT package. Each Orb's service is named
/// `<orb>::<interface identity>`, as for a component.
const GREETING: &str = "example:greeter/greeting@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    // The Orbs the crate manifests name, `english` and `italian`, each with the signature of the
    // shared interface, so the kernel checks both against one contract.
    host.install(vec![english::orb()?, italian::orb()?])?;
    let main = |orb: &str| {
        let service = format!("{orb}::{GREETING}");
        host.get(&service, vec!["main".into()], "alice")
            .map(Rc::new)
    };
    let in_english = english::greeter::GreetingClient::new(main("english")?);
    let in_italian = italian::greeter::GreetingClient::new(main("italian")?);
    let (hello, ciao) = pool.run_until(host.root("alice", move |context| {
        Box::pin(async move {
            // Each call takes the local entrypoint with Rust values: no JSON crosses.
            let hello = in_english.hello(&context, "Ada".into()).await?;
            let ciao = in_italian.hello(&context, "Ada".into()).await?;
            Ok::<_, tembra::Error>((hello, ciao))
        })
    }))?;
    println!("english::{GREETING}  hello(\"Ada\") -> {hello}");
    println!("italian::{GREETING}  hello(\"Ada\") -> {ciao}");
    pool.run_until(host.close())?;
    // Cleanup failures are reported here, not as call errors.
    let errors = host.errors();
    if !errors.is_empty() {
        return Err(format!("host errors: {errors:?}").into());
    }
    Ok(())
}
