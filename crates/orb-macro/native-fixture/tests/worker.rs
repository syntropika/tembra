//! `worker::orb(..)` in a native host: an Orb whose only content is its `#[root]`, which runs a
//! worker while the host keeps the Orb installed.
use futures::executor::LocalPool;
use std::{cell::RefCell, rc::Rc};
use tembra::{Code, Error, Host, Orb, contracts::Contract};
use tembra_orb_native_fixture::worker;

/// What the worker did, in order, as it reports it to the host.
type Events = Rc<RefCell<Vec<String>>>;

/// The Orb, whose worker reports to `events`, and whose root fails with `failure`, if any.
fn orb(events: &Events, failure: Option<Error>) -> Orb {
    worker::orb(worker::Inputs {
        events: events.clone(),
        failure,
    })
    .unwrap()
}

/// The Orb exports nothing: it declares no service, as the plan of its contract, an empty world,
/// does not. Installed, its root starts the worker, which runs once the host's executor polls it
/// and stops when the host closes. While the worker runs, the host refuses to reload the Orb,
/// which would run the root again: the worker's task cannot be restarted.
#[test]
fn the_root_runs_a_worker_while_the_orb_is_installed() {
    let events = Events::default();
    let orb = orb(&events, None);
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
    let error = pool
        .run_until(host.reload(vec![self::orb(&events, None)], 5000))
        .unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Unsupported, "task_restart"),
        "{error}"
    );
    pool.run_until_stalled();
    assert_eq!(*events.borrow(), ["run"]);
    pool.run_until(host.close()).unwrap();
    assert_eq!(*events.borrow(), ["run", "stop"]);
}

/// The host commits the Orb before it runs the root: a root that fails makes `install` return its
/// error, and the worker never starts, but the Orb stays installed, so installing it again is
/// refused.
#[test]
fn a_failing_root_fails_the_installation_and_leaves_the_orb_installed() {
    let events = Events::default();
    let failure = Error::new(Code::Unavailable, "worker", "the worker cannot start");
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    let error = host
        .install(vec![orb(&events, Some(failure.clone()))])
        .unwrap_err();
    assert_eq!(error, failure);
    let installed: Vec<String> = host
        .kernel()
        .installed_orbs()
        .into_iter()
        .map(|orb| orb.declaration.name)
        .collect();
    assert_eq!(installed, ["calculator"]);
    let error = host.install(vec![orb(&events, None)]).unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (Code::Invalid, "orb_installed"),
        "{error}"
    );
    pool.run_until_stalled();
    assert!(events.borrow().is_empty(), "{:?}", events.borrow());
    pool.run_until(host.close()).unwrap();
}
