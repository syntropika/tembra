use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};
use tembra_core::*;

struct Binding {
    drops: Rc<Cell<usize>>,
}
impl Drop for Binding {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}
fn anchor(declaration: &FacetDeclaration<'_>, drops: &Rc<Cell<usize>>) -> Weak<Binding> {
    let binding = Rc::new(Binding {
        drops: drops.clone(),
    });
    let weak = Rc::downgrade(&binding);
    declaration.recorder().retain_run_owner(binding).unwrap();
    weak
}

#[test]
fn installed_anchor_survives_setup_but_expired_recorder_stores_nothing() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("run-anchor", json!(null)), driver.clone());
    let drops = Rc::new(Cell::new(0));
    let weak = Rc::new(RefCell::new(Weak::<Binding>::new()));
    let saved = Rc::new(RefCell::new(None));
    let (observed, recorded, recorder) = (drops.clone(), weak.clone(), saved.clone());
    let facet = NativeFacet::new("anchor", vec![], move |_, declaration| {
        *recorded.borrow_mut() = anchor(declaration, &observed);
        *recorder.borrow_mut() = Some(declaration.recorder());
        Ok(())
    })
    .unwrap();
    driver
        .run_until(host.install(facet, LoadOptions::default()).unwrap())
        .unwrap();
    assert!(weak.borrow().upgrade().is_some());
    let refused = Rc::new(Cell::new(0));
    let error = saved
        .borrow()
        .as_ref()
        .unwrap()
        .retain_run_owner(Rc::new(Binding {
            drops: refused.clone(),
        }))
        .unwrap_err();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (ErrorCode::Invalid, "setup_scope")
    );
    assert_eq!(refused.get(), 1);
    assert_eq!(drops.get(), 0);
    driver.run_until(host.close()).unwrap();
    assert!(weak.borrow().upgrade().is_none());
    assert_eq!(drops.get(), 1);
}

#[test]
fn dropping_host_releases_observer_only_run_without_polling_driver() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("weak-run", json!(null)), driver.clone());
    let drops = Rc::new(Cell::new(0));
    let weak = Rc::new(RefCell::new(Weak::<Binding>::new()));
    let (observed, recorded) = (drops.clone(), weak.clone());
    let facet = NativeFacet::new("anchor", vec![], move |_, declaration| {
        *recorded.borrow_mut() = anchor(declaration, &observed);
        Ok(())
    })
    .unwrap();
    drop(
        host.install(
            facet,
            LoadOptions {
                await_host_objects: false,
            },
        )
        .unwrap(),
    );
    assert!(weak.borrow().upgrade().is_some());
    drop(host);
    assert!(weak.borrow().upgrade().is_none());
    assert_eq!(drops.get(), 1);
}

fn foreign(
    name: &str,
    drops: Rc<Cell<usize>>,
    weak: Rc<RefCell<Weak<Binding>>>,
    fail: bool,
) -> ExecutableFacet {
    ExecutableFacet::foreign(name, vec![], move |_, declaration| {
        *weak.borrow_mut() = anchor(declaration, &drops);
        if fail {
            return Err(RuntimeError::invalid("candidate_fixture", "refused setup"));
        }
        Ok(())
    })
    .unwrap()
}

#[test]
fn refused_candidate_drops_its_anchor_and_commit_retires_only_the_old_run() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("replacement", json!(null)), driver.clone());
    let old_drops = Rc::new(Cell::new(0));
    let old = Rc::new(RefCell::new(Weak::new()));
    driver
        .run_until(
            host.install_executable(
                foreign("run", old_drops.clone(), old.clone(), false),
                LoadOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
    let candidate_drops = Rc::new(Cell::new(0));
    let candidate = Rc::new(RefCell::new(Weak::new()));
    let error = driver
        .run_until(host.reload(
            vec![foreign(
                "run",
                candidate_drops.clone(),
                candidate.clone(),
                true,
            )],
            ReloadOptions::default(),
        ))
        .unwrap_err();
    assert_eq!(error.reason, "candidate_fixture");
    assert!(old.borrow().upgrade().is_some());
    assert!(candidate.borrow().upgrade().is_none());
    assert_eq!((old_drops.get(), candidate_drops.get()), (0, 1));
    let next_drops = Rc::new(Cell::new(0));
    let next = Rc::new(RefCell::new(Weak::new()));
    driver
        .run_until(host.reload(
            vec![foreign("run", next_drops.clone(), next.clone(), false)],
            ReloadOptions::default(),
        ))
        .unwrap();
    assert!(old.borrow().upgrade().is_none());
    assert!(next.borrow().upgrade().is_some());
    assert_eq!((old_drops.get(), next_drops.get()), (1, 0));
    driver.run_until(host.close()).unwrap();
    assert!(next.borrow().upgrade().is_none());
    assert_eq!(next_drops.get(), 1);
}

struct Reenter(Option<Box<dyn FnOnce()>>);
impl Drop for Reenter {
    fn drop(&mut self) {
        if let Some(action) = self.0.take() {
            action();
        }
    }
}

#[test]
fn refused_setup_retires_storage_before_owner_drop_reenters_install() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("reentrant-drop", json!(null)),
        driver.clone(),
    );
    let entered = Rc::new(Cell::new(false));
    let observed = entered.clone();
    let captured = host.clone();
    let facet = NativeFacet::new("run", vec![], move |scope, declaration| {
        let host = captured.clone();
        let signal = scope.signal();
        let observed = observed.clone();
        declaration
            .recorder()
            .retain_run_owner(Rc::new(Reenter(Some(Box::new(move || {
                assert!(signal.is_cancelled());
                drop(
                    host.install(
                        NativeFacet::new("run", vec![], |_, _| Ok(())).unwrap(),
                        LoadOptions::default(),
                    )
                    .unwrap(),
                );
                observed.set(true);
            })))))?;
        Err(RuntimeError::invalid("setup_fixture", "refused setup"))
    })
    .unwrap();
    let error = host.install(facet, LoadOptions::default()).err().unwrap();
    assert_eq!(error.reason, "setup_fixture");
    assert!(entered.get());
    driver.run_until(host.close()).unwrap();
}
