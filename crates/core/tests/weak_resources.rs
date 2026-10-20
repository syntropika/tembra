use futures::FutureExt;
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{contracts::Contract, *};

struct Payload(u64);

#[test]
fn weak_resource_preserves_live_guards_and_does_not_retain_record_callbacks() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("weak-resource", json!(null)),
        driver.clone(),
    );
    let contract = Rc::new(Contract::local("owner", 1, json!({}), BTreeMap::new()).unwrap());
    let saved = Rc::new(RefCell::new(None::<WeakResource<Payload>>));
    let closed = Rc::new(Cell::new(0));
    let sentinel = Rc::new(());
    let released = Rc::downgrade(&sentinel);
    let callback_capture = Rc::new(RefCell::new(Some(sentinel.clone())));
    drop(sentinel);
    let (handle, closes) = (saved.clone(), closed.clone());
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let captured = callback_capture
                .borrow_mut()
                .take()
                .expect("one activation in this test");
            let closes = closes.clone();
            let mut definition = ResourceDeclaration::new(move |_| {
                let _keep = &captured;
                async { Ok(Payload(7)) }.boxed_local()
            })
            .with_close(move |value| {
                assert_eq!(value.0, 7);
                closes.set(closes.get() + 1);
                async { Ok(()) }.boxed_local()
            });
            definition.version = 3;
            definition.exclusive = true;
            let resource = declaration.resource("payload", definition)?;
            let weak = resource.downgrade();
            assert_eq!(weak.upgrade()?.value().err().unwrap().reason, "factory");
            *handle.borrow_mut() = Some(weak.clone());
            // Dependency conversion still uses the same real record and declaration check.
            let mut dependent = ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local());
            dependent.deps.push(weak.upgrade()?.dependency());
            declaration.resource("dependent", dependent)?;
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    let weak = saved.borrow().clone().unwrap();
    {
        let temporary = weak.upgrade().unwrap();
        assert_eq!(temporary.version(), 3);
        assert!(temporary.exclusive());
        assert_eq!(temporary.value().unwrap().0, 7);
    }
    assert!(released.upgrade().is_some());
    reference.release();
    driver.run_until(host.close()).unwrap();
    assert_eq!(closed.get(), 1);
    assert!(
        released.upgrade().is_none(),
        "weak adapter handle does not retain callback captures"
    );
    let error = weak.upgrade().err().unwrap();
    assert_eq!(
        (error.code, error.reason.as_str()),
        (ErrorCode::Gone, "resource")
    );
}

#[test]
fn weak_upgrade_of_separately_retained_closed_record_does_not_revive_its_value() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("closed-resource", json!(null)),
        driver.clone(),
    );
    let contract = Rc::new(Contract::local("owner", 1, json!({}), BTreeMap::new()).unwrap());
    let strong = Rc::new(RefCell::new(None::<Resource<Payload>>));
    let handle = strong.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let mut definition =
                ResourceDeclaration::new(|_| async { Ok(Payload(9)) }.boxed_local());
            definition.version = 2;
            *handle.borrow_mut() = Some(declaration.resource("payload", definition)?);
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    let weak = strong.borrow().as_ref().unwrap().downgrade();
    assert_eq!(weak.upgrade().unwrap().value().unwrap().0, 9);
    reference.release();
    driver.run_until(host.close()).unwrap();
    {
        let temporary = weak.upgrade().unwrap();
        assert_eq!(temporary.version(), 2);
        assert!(!temporary.exclusive());
        let error = temporary.value().err().unwrap();
        assert_eq!(
            (error.code, error.reason.as_str()),
            (ErrorCode::NotReady, "resource")
        );
    }
    let retained = strong.borrow_mut().take();
    drop(retained);
    assert_eq!(weak.upgrade().err().unwrap().code, ErrorCode::Gone);
}
