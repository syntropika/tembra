use futures::FutureExt;
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};

struct Capture {
    drops: Rc<Cell<usize>>,
    on_drop: Option<Box<dyn Fn()>>,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
        if let Some(callback) = &self.on_drop {
            callback();
        }
    }
}
fn token(name: &str, callable: bool) -> Rc<Contract> {
    let members = if callable {
        BTreeMap::from([(
            "run".into(),
            Member::Method {
                args: None,
                result: None,
            },
        )])
    } else {
        BTreeMap::new()
    };
    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

#[test]
fn closed_factory_captures_do_not_depend_on_released_reference_wrapper_drop() {
    for physical in [false, true] {
        let driver = ManualDriver::new();
        let host = Host::new(HostOptions::new("factory", json!(null)), driver.clone());
        let contract = token("factory", physical);
        let drops = Rc::new(Cell::new(0));
        let entered = Rc::new(Cell::new(0));
        let lease = Rc::new(RefCell::new(None::<InvocationLease>));
        let capture = Capture {
            drops: drops.clone(),
            on_drop: None,
        };
        let calls = entered.clone();
        let retained = lease.clone();
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let _capture = &capture;
                calls.set(calls.get() + 1);
                let owner = declaration.invocation_owner();
                let retained = retained.clone();
                Ok(if physical {
                    Implementation::new().method("run", move |(): (), context| {
                        *retained.borrow_mut() = Some(owner.retain(&context).unwrap());
                        async { Ok(()) }.boxed_local()
                    })
                } else {
                    Implementation::new()
                })
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let reference = driver
            .run_until(host.get(contract, json!({}), GetOptions::default()))
            .unwrap();
        if physical {
            driver
                .run_until(reference.call::<(), ()>("run", (), None))
                .unwrap();
        }
        reference.release();
        let retained_wrapper = reference.clone();
        let mut closing = host.close().boxed_local();
        if physical {
            assert!(closing.as_mut().now_or_never().is_none());
            driver.run_until_stalled();
            assert_eq!(drops.get(), 0, "physical work still retains cleanup");
            drop(lease.borrow_mut().take());
        }
        driver.run_until(closing).unwrap();
        assert_eq!(entered.get(), 1);
        assert_eq!(
            drops.get(),
            1,
            "released wrappers are addresses, not factory owners"
        );
        drop((reference, retained_wrapper));
        assert_eq!(drops.get(), 1);
    }
}

#[test]
fn canceled_unstarted_factory_is_released_without_entering_user_code() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("unstarted", json!(null)), driver.clone());
    let contract = token("unstarted", false);
    let drops = Rc::new(Cell::new(0));
    let calls = Rc::new(Cell::new(0));
    let capture = Capture {
        drops: drops.clone(),
        on_drop: None,
    };
    let entered = calls.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, _| {
            let _capture = &capture;
            entered.set(entered.get() + 1);
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = host
        .hold(contract, json!({}), GetOptions::default())
        .unwrap();
    reference.release();
    driver.run_until(host.close()).unwrap();
    assert_eq!(calls.get(), 0);
    assert_eq!(drops.get(), 1);
    drop(reference);
    assert_eq!(drops.get(), 1);
}

#[test]
fn failed_and_unwound_setup_release_unstarted_factories_before_wrappers() {
    for panics in [false, true] {
        let driver = ManualDriver::new();
        let host = Host::new(HostOptions::new("withdraw", json!(null)), driver.clone());
        let helper = token("helper", false);
        host.serve(
            helper.clone(),
            Rc::new(|_, _, _| Ok(Implementation::new())),
            ServeOptions::default(),
        )
        .unwrap();
        let contract = token("withdraw", false);
        let saved = Rc::new(RefCell::new(None::<Reference>));
        let reentered = Rc::new(RefCell::new(None::<Reference>));
        let drops = Rc::new(Cell::new(0));
        let calls = Rc::new(Cell::new(0));
        let (captured_host, saved_ref, count, entered, from_drop) = (
            host.clone(),
            saved.clone(),
            drops.clone(),
            calls.clone(),
            reentered.clone(),
        );
        let facet = NativeFacet::new("withdraw", vec![], move |_, declaration| {
            let (on_drop_host, helper, from_drop) =
                (captured_host.clone(), helper.clone(), from_drop.clone());
            let capture = Capture {
                drops: count.clone(),
                on_drop: Some(Box::new(move || {
                    *from_drop.borrow_mut() = Some(
                        on_drop_host
                            .hold(helper.clone(), json!({}), GetOptions::default())
                            .unwrap(),
                    );
                })),
            };
            let entered = entered.clone();
            declaration.serve(
                contract.clone(),
                Rc::new(move |_, _, _| {
                    let _capture = &capture;
                    entered.set(entered.get() + 1);
                    Ok(Implementation::new())
                }),
                ServeOptions::default(),
            )?;
            *saved_ref.borrow_mut() =
                Some(captured_host.hold(contract.clone(), json!({}), GetOptions::default())?);
            if panics {
                panic!("native setup panic");
            }
            Err(RuntimeError::invalid("setup_test", "setup refused"))
        })
        .unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            host.install(facet, LoadOptions::default())
        }));
        if panics {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_err());
        }
        assert_eq!(calls.get(), 0);
        assert_eq!(
            drops.get(),
            1,
            "withdrawn factory must not await retained Claim destruction"
        );
        let reference = saved.borrow_mut().take().unwrap();
        let error = driver.run_until(reference.ready()).unwrap_err();
        assert_eq!(
            error.reason,
            if panics {
                "setup_unwound"
            } else {
                "setup_test"
            }
        );
        let helper = reentered.borrow_mut().take().unwrap();
        // A destructor can declare work while unwinding; the next safe Host boundary flushes it.
        driver.run_until(host.ready()).unwrap();
        driver.run_until(helper.ready()).unwrap();
        reference.release();
        helper.release();
        driver.run_until(host.close()).unwrap();
        assert_eq!(drops.get(), 1);
    }
}
