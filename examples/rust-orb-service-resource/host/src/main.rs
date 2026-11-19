//! Install the harbor, moor two ships, each with a mooring of its own, see the third refused when
//! every berth is taken, release a ship and watch the kernel close its mooring, then close the
//! host. The same steps run on the compiled Orb, loaded from the artifact given, and on the native
//! Orb the module's `orb()` builds, with `--native`: both print the same lines. No Orb is defined
//! here.
//!
//! The harbor logs what the moorings go through; the host reads the log through `log` after each
//! step, so that the order of opens and closes shows between the calls.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::rc::Rc;
use tembra::wasm::{Engine, Install, Limits};
use tembra::{Host, Reference};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argument = std::env::args()
        .nth(1)
        .ok_or("usage: host <path to harbor.orb.wasm> | host --native")?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    // The world imports the kernel's resources interface, which the executor serves itself:
    // nothing to require, nothing to grant. Natively the Orb is the module's `orb()`.
    let (orb, name) = if argument == "--native" {
        (tembra_example_harbor::port::orb()?, "harbor")
    } else {
        let engine = Engine::new(Limits::default())?;
        (
            engine.load(&std::fs::read(&argument)?, Install::default())?,
            "harbor",
        )
    };
    host.install(vec![orb])?;
    let ship_service = format!("{name}::example:harbor/ship@1.0.0");

    // 1. Two ships: creating each opens its mooring, after the harbor, which opens once.
    let one = moor(&mut pool, &host, &ship_service, "one")?;
    let two = moor(&mut pool, &host, &ship_service, "two")?;
    log(&mut pool, &host, &one)?;
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
    match moor(&mut pool, &host, &ship_service, "three") {
        Ok(_) => return Err("the third ship moored".into()),
        Err(error) => println!("ship three -> {error}"),
    }

    // 3. Releasing a ship closes its mooring, which gives its berth back; the other ship keeps its
    //    mooring and its cargo, and a new ship moors at a fresh berth.
    one.release()?;
    pool.run_until(host.settled());
    println!("released one");
    log(&mut pool, &host, &two)?;
    println!(
        "load(two, 1) -> {}",
        call(&mut pool, &host, &two, "load", json!([1]))?
    );
    let three = moor(&mut pool, &host, &ship_service, "three")?;
    log(&mut pool, &host, &three)?;
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
fn moor(
    pool: &mut LocalPool,
    host: &Host,
    service: &str,
    key: &str,
) -> tembra::Result<Rc<Reference>> {
    let ship = Rc::new(host.get(service, vec![key.into()], "alice")?);
    pool.run_until(ship.ready())?;
    println!("ship {key} moored");
    Ok(ship)
}

/// Print what the moorings went through since the last reading, which any ship answers.
fn log(pool: &mut LocalPool, host: &Host, ship: &Rc<Reference>) -> tembra::Result<()> {
    for line in call(pool, host, ship, "log", json!([]))?
        .as_array()
        .into_iter()
        .flatten()
    {
        println!("  log: {}", line.as_str().unwrap_or_default());
    }
    Ok(())
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
