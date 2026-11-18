//! The wiring: install the compiled keeper and a native keeper of the same shared interface, then
//! consult both the same way. Which Orb keeps a book is the installer's choice; the contract is
//! the WIT package, and the two implementations are interchangeable behind it.
use futures::executor::LocalPool;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use tembra::wasm::{Contract, Engine, Install, Limits};
use tembra::{Host, Orb, Reference, Service};

/// The shared interface, implemented by both keepers.
const BOOK: &str = "example:ledger/book@1.0.0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact = std::env::args()
        .nth(1)
        .ok_or("usage: host <path to book.orb.wasm>")?;
    let engine = Engine::new(Limits::default())?;
    // 1. The compiled keeper: its artifact exports the shared interface, nothing to grant.
    let compiled = engine.load(&std::fs::read(&artifact)?, Install::default())?;
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![compiled, native_keeper()?])?;

    // 2. Both keepers answer the same methods under the same service name of their Orb.
    for orb in ["book", "notebook"] {
        let keeper = Rc::new(host.get(&format!("{orb}::{BOOK}"), vec!["main".into()], "alice")?);
        pool.run_until(keeper.ready())?;
        for (label, amount) in [("rent", -900), ("salary", 2_400)] {
            let count = call(
                &mut pool,
                &host,
                &keeper,
                "add",
                json!([{ "label": label, "amount": amount }]),
            )?;
            println!("{orb:<9} add({label}, {amount}) -> {count}");
        }
        println!(
            "{orb:<9} total() -> {}",
            call(&mut pool, &host, &keeper, "total", json!([]))?
        );
        println!(
            "{orb:<9} entries() -> {}",
            call(&mut pool, &host, &keeper, "entries", json!([]))?
        );
        keeper.release()?;
    }
    pool.run_until(host.close())?;
    // Cleanup failures are reported here, not as call errors.
    let errors = host.errors();
    if !errors.is_empty() {
        return Err(format!("host errors: {errors:?}").into());
    }
    Ok(())
}

/// A native keeper of the same interface, declared with the `tembra` facade and the WIT signature
/// of the shared package, so the kernel checks it against the same contract.
fn native_keeper() -> Result<Orb, Box<dyn std::error::Error>> {
    let contract = Contract::from_dir(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../wit"),
        Some("keeper"),
    )?;
    let service = contract
        .plan("notebook", 1, &Default::default())?
        .orb
        .services
        .remove(0);
    let entries: Rc<RefCell<Vec<Value>>> = Rc::default();
    let (added, summed, listed) = (entries.clone(), entries.clone(), entries);
    Ok(Orb::new("notebook", 1).service(
        Service::new(service.name, |_| Ok(()))
            .signature(service.signature)
            .method("add", move |_, _, (entry,): (Value,)| {
                let added = added.clone();
                async move {
                    added.borrow_mut().push(entry);
                    Ok(added.borrow().len() as u32)
                }
            })
            // A zero-argument portable call carries an empty list, as the compiled keeper's does.
            .method("total", move |_, _, _: [Value; 0]| {
                let summed = summed.clone();
                async move {
                    Ok(summed
                        .borrow()
                        .iter()
                        .map(|entry| entry["amount"].as_i64().unwrap_or_default())
                        .sum::<i64>())
                }
            })
            .method("entries", move |_, _, _: [Value; 0]| {
                let listed = listed.clone();
                async move { Ok(listed.borrow().clone()) }
            }),
    ))
}

/// Call one method under a root context of the principal `alice`.
fn call(
    pool: &mut LocalPool,
    host: &Host,
    reference: &Rc<Reference>,
    method: &'static str,
    arguments: Value,
) -> tembra::Result<Value> {
    let reference = reference.clone();
    pool.run_until(host.root("alice", move |context| {
        Box::pin(async move { reference.call_portable(&context, method, arguments).await })
    }))
}
