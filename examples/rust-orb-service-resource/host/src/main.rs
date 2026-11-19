//! Install the harbor, moor two ships, each with a mooring of its own, see the third refused when
//! every berth is taken, release a ship and watch the kernel close its mooring, then close the
//! host and watch the moorings close before the harbor. The same steps run on the compiled Orb,
//! loaded from the artifact given, and on the native Orb the module's `orb()` builds, with
//! `--native`: both print the same lines. No Orb is defined here.
//!
//! The resources log what they go through, through the kernel's context; the host prints each
//! record as it comes, from the engine for the compiled Orb and from the host for the native one,
//! so that the order of opens and closes shows between the calls.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::wasm::{Engine, Install, Limits};
use tembra::{Host, Reference};

/// The service of the Orb `harbor`, named after its exported interface.
const SHIP: &str = "harbor::example:harbor/ship@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argument = std::env::args()
        .nth(1)
        .ok_or("usage: host <path to harbor.orb.wasm> | host --native")?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    // The world imports the kernel's resources and context interfaces, which the executor serves
    // itself: nothing to require, nothing to grant. Natively the Orb is the module's `orb()`, and
    // the host's log observer takes what the compiled Orb's engine takes.
    let orb = if argument == "--native" {
        host.on_log(|record| println!("  log: {}", record.message));
        tembra_example_harbor::port::orb()?
    } else {
        let engine =
            Engine::new(Limits::default())?.on_log(|record| println!("  log: {}", record.message));
        engine.load(&std::fs::read(&argument)?, Install::default())?
    };
    host.install(vec![orb])?;

    // 1. Two ships: creating each opens its mooring, after the harbor, which opens once.
    let one = moor(&mut pool, &host, "one")?;
    let two = moor(&mut pool, &host, "two")?;
    println!(
        "berth(one) -> {}   berth(two) -> {}",
        call(&mut pool, &host, &one, "berth", json!([]))?,
        call(&mut pool, &host, &two, "berth", json!([]))?
    );
    // Each ship's mooring keeps its own cargo.
    println!(
        "load(one, 3) -> {}   load(one, 2) -> {}   load(two, 1) -> {}",
        call(&mut pool, &host, &one, "load", json!([3]))?,
        call(&mut pool, &host, &one, "load", json!([2]))?,
        call(&mut pool, &host, &two, "load", json!([1]))?
    );

    // 2. A third ship finds every berth taken: its mooring's `#[open]` answers an error, which
    //    fails the creation of that ship alone.
    match moor(&mut pool, &host, "three") {
        Ok(_) => return Err("the third ship moored".into()),
        Err(error) => println!("ship three -> {error}"),
    }

    // 3. Releasing a ship closes its mooring, which gives its berth back; the other ship keeps its
    //    mooring and its cargo, and a new ship moors at a fresh berth.
    one.release()?;
    pool.run_until(host.settled());
    println!("released one");
    println!(
        "load(two, 1) -> {}",
        call(&mut pool, &host, &two, "load", json!([1]))?
    );
    let three = moor(&mut pool, &host, "three")?;
    println!(
        "berth(three) -> {}",
        call(&mut pool, &host, &three, "berth", json!([]))?
    );

    // 4. Closing the host closes the moorings of the remaining ships, then the harbor.
    two.release()?;
    three.release()?;
    pool.run_until(host.close())?;
    // Cleanup failures (a resource closer) are reported here, not as call errors.
    let errors = host.errors();
    if !errors.is_empty() {
        return Err(format!("host errors: {errors:?}").into());
    }
    Ok(())
}

/// A reference to the ship `key`, acquired by `alice`, once the kernel created it and opened its
/// mooring, or the error of a mooring that did not open.
fn moor(pool: &mut LocalPool, host: &Host, key: &str) -> tembra::Result<Rc<Reference>> {
    let ship = Rc::new(host.get(SHIP, vec![key.into()], "alice")?);
    pool.run_until(ship.ready())?;
    println!("ship {key} moored");
    Ok(ship)
}

/// Call one method of a ship under a root context of the principal `alice`.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    ship: &Rc<Reference>,
    method: &'static str,
    arguments: Value,
) -> tembra::Result<Value> {
    let ship = ship.clone();
    pool.run_until(host.root("alice", move |context| {
        Box::pin(async move { ship.call_portable(&context, method, arguments).await })
    }))
}
