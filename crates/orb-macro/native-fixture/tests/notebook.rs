//! `notebook::orb(..)` in a native host: the Orb takes the host's inputs.
use futures::{FutureExt, executor::LocalPool};
use std::{cell::RefCell, rc::Rc};
use tembra::{Host, contracts::Contract};
use tembra_orb_native_fixture::notebook;

const NOTES: &str = "calculator::example:notes/notes@1.0.0";

/// The inputs a host lends the Orb: the log of what happened, and a title per address.
fn inputs(events: &Rc<RefCell<Vec<String>>>) -> notebook::Inputs {
    notebook::Inputs {
        events: events.clone(),
        title: Box::new(|address| format!("{} of {}", address.key.join("/"), address.principal)),
    }
}

/// `orb(..)` takes the Orb's `#[inputs]`, which stay out of the WIT: the Orb declares the plan of
/// the module's contract, as `calculator::orb()` does, and a host installs it and calls it.
#[test]
fn orb_takes_the_inputs() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let orb = notebook::orb(inputs(&events)).unwrap();
    let expected = Contract::from_source("notebook.wit", notebook::WIT, Some("notebook"))
        .unwrap()
        .plan("calculator", 2, &Default::default())
        .unwrap()
        .orb;
    assert_eq!(orb.declaration(), &expected);
    assert!(!notebook::WIT.contains("record"), "{}", notebook::WIT);
    let mut pool = LocalPool::new();
    let host = Host::new(pool.spawner());
    host.install(vec![orb]).unwrap();
    let notes = Rc::new(host.get(NOTES, vec!["main".into()], "host").unwrap());
    let called = notes.clone();
    let echoed = pool
        .run_until(host.root("host", move |context| {
            async move {
                called
                    .call::<_, String>(&context, "echo", ("hi".to_string(),))
                    .await
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(echoed, "hi");
    notes.release().unwrap();
    pool.run_until(host.close()).unwrap();
}
