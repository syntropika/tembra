//! `worker::orb(..)` in a native host: an Orb whose only content is its `#[root]`, which runs a
//! worker while the host keeps the Orb installed.
use futures::executor::LocalPool;
use std::{cell::RefCell, rc::Rc};
use tembra::{Host, contracts::Contract};
use tembra_orb_native_fixture::worker;

/// The Orb exports nothing: it declares no service, as the plan of its contract, an empty world,
/// does not. Installed, its root starts the worker, which runs once the host's executor polls it
/// and stops when the host closes.
#[test]
fn the_root_runs_a_worker_while_the_orb_is_installed() {
    let events = Rc::new(RefCell::new(Vec::<String>::new()));
    let orb = worker::orb(worker::Inputs {
        events: events.clone(),
    })
    .unwrap();
    let expected = Contract::from_source("worker.wit", worker::WIT, Some("worker"))
        .unwrap()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    assert_eq!(orb.declaration(), &expected);
    assert!(orb.declaration().services.is_empty());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    pool.run_until_stalled();
    assert_eq!(*events.borrow(), ["run"]);
    pool.run_until(host.close()).unwrap();
    assert_eq!(*events.borrow(), ["run", "stop"]);
}
