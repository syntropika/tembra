//! `worker::orb(..)` in a native host: an Orb whose only content is its `#[root]`, which runs a
//! worker while the host keeps the Orb installed, and a resource of the root, the worker's lamp,
//! lit as long.
use futures::executor::LocalPool;
use std::{cell::RefCell, rc::Rc};
use tembra::{
    Code, Error, Host, Orb,
    contracts::{Binding, Contract, guest},
};
use tembra_orb_native_fixture::{journal, worker};

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

/// The Orb exports nothing: it declares no service, as the plan of its contract, an empty world
/// whose import of the kernel's resources interface binds to the kernel, does not. Installed, its
/// create declares the lamp and runs the root, which starts the worker: the host lights the lamp
/// once the create returns, and the worker runs once the host's executor polls it; the worker
/// stops and the lamp goes out when the host closes. While the worker runs, the host refuses to
/// reload the Orb, which would run the root again: the worker's task cannot be restarted, and the
/// lamp stays lit.
#[test]
fn the_root_runs_a_worker_while_the_orb_is_installed() {
    journal();
    let events = Events::default();
    let orb = orb(&events, None);
    let kernel = [(guest::RESOURCE.to_owned(), Binding::Kernel)].into();
    let expected = Contract::from_source("worker.wit", worker::WIT, Some("worker"))
        .unwrap()
        .plan("calculator", 2, &kernel)
        .unwrap()
        .orb;
    assert_eq!(orb.declaration(), &expected);
    assert!(orb.declaration().services.is_empty());
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    pool.run_until_stalled();
    assert_eq!(*events.borrow(), ["run"]);
    assert_eq!(journal(), ["lamp lit"]);
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
    assert!(journal().is_empty());
    pool.run_until(host.close()).unwrap();
    assert_eq!(*events.borrow(), ["run", "stop"]);
    assert_eq!(journal(), ["lamp out"]);
}

/// The host commits the Orb before it runs the root: a root that fails makes `install` return its
/// error, and the worker never starts, nor is the lamp the create declared before it ever lit, but
/// the Orb stays installed, so installing it again is refused.
#[test]
fn a_failing_root_fails_the_installation_and_leaves_the_orb_installed() {
    journal();
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
    assert!(journal().is_empty());
}
