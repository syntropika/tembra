//! A native task that fails is reported in `Host::errors`, whether it returns an error or panics.
use futures::{FutureExt, executor::LocalPool};
use tembra::{Error, Host, Orb, Service};

/// A service whose objects declare a task that ends as `end` has it: an error or a panic.
fn orb(end: fn() -> Error) -> Orb {
    Orb::new("tasks", 1).service(Service::new("worker", move |declaration| {
        declaration.task(move |_| async move { Err(end()) }.boxed_local())
    }))
}

/// The errors the host reports once the task of one object has run.
fn reported(end: fn() -> Error) -> Vec<Error> {
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb(end)]).unwrap();
    let worker = host.get("worker", vec!["main".into()], "alice").unwrap();
    pool.run_until(worker.ready()).unwrap();
    pool.run_until_stalled();
    let errors = host.errors();
    worker.release().unwrap();
    pool.run_until(host.close()).unwrap();
    errors
}

#[test]
fn a_task_that_returns_an_error_is_reported() {
    let errors = reported(|| Error::invalid("worker", "the task failed"));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "the task failed");
}

#[test]
fn a_task_that_panics_is_reported() {
    let errors = reported(|| panic!("the task panicked"));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].reason, "callback");
    assert_eq!(errors[0].message, "native callback panicked");
}
