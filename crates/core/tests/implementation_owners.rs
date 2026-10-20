use futures::FutureExt;
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use tembra_core::{
    contracts::{Contract, Member},
    *,
};

struct Owner {
    drops: Rc<Cell<u32>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

#[test]
fn empty_and_physically_retained_implementations_keep_owners_through_resource_close() {
    for callable in [false, true] {
        let driver = ManualDriver::new();
        let host = Host::new(HostOptions::new("owners", json!(null)), driver.clone());
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
        let contract = Rc::new(Contract::local("owner", 1, json!({}), members).unwrap());
        let dropped = Rc::new(Cell::new(0));
        let closed = Rc::new(Cell::new(0));
        let observer = Rc::new(RefCell::new(None));
        let physical = Rc::new(RefCell::new(None::<InvocationLease>));
        let (drops, closes, weak, job) = (
            dropped.clone(),
            closed.clone(),
            observer.clone(),
            physical.clone(),
        );
        host.serve(
            contract.clone(),
            Rc::new(move |_, _, declaration| {
                let owner = Rc::new(Owner {
                    drops: drops.clone(),
                });
                let weak_owner = Rc::downgrade(&owner);
                *weak.borrow_mut() = Some(weak_owner.clone());
                let closes = closes.clone();
                declaration.resource(
                    "resource",
                    ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                        move |_| {
                            assert!(
                                weak_owner.upgrade().is_some(),
                                "implementation owner survives resource cleanup"
                            );
                            closes.set(closes.get() + 1);
                            async { Ok(()) }.boxed_local()
                        },
                    ),
                )?;
                let implementation = Implementation::new().retain_owner(owner);
                Ok(if callable {
                    let invocation = declaration.invocation_owner();
                    let job = job.clone();
                    implementation.method("run", move |(): (), context| {
                        *job.borrow_mut() = Some(invocation.retain(&context).unwrap());
                        async { Ok(()) }.boxed_local()
                    })
                } else {
                    implementation
                })
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let reference = driver
            .run_until(host.get(contract, json!({}), GetOptions::default()))
            .unwrap();
        assert!(observer.borrow().as_ref().unwrap().upgrade().is_some());
        assert_eq!(dropped.get(), 0);
        if callable {
            driver
                .run_until(reference.call::<(), ()>("run", (), None))
                .unwrap();
        }
        reference.release();
        let mut closing = host.close().boxed_local();
        if callable {
            assert!(closing.as_mut().now_or_never().is_none());
            driver.run_until_stalled();
            assert_eq!(closed.get(), 0);
            assert_eq!(dropped.get(), 0);
            let lease = physical.borrow_mut().take();
            drop(lease);
        }
        driver.run_until(closing).unwrap();
        assert_eq!(closed.get(), 1);
        assert_eq!(dropped.get(), 1);
        assert!(observer.borrow().as_ref().unwrap().upgrade().is_none());
    }
}

#[test]
fn refused_implementation_releases_its_owner_without_invoking_its_method() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("refused", json!(null)), driver.clone());
    let contract = Rc::new(Contract::local("empty", 1, json!({}), BTreeMap::new()).unwrap());
    let drops = Rc::new(Cell::new(0));
    let observed = drops.clone();
    let calls = Rc::new(Cell::new(0));
    let invoked = calls.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, _| {
            let invoked = invoked.clone();
            Ok(Implementation::new()
                .retain_owner(Rc::new(Owner {
                    drops: observed.clone(),
                }))
                .method("extra", move |(): (), _| {
                    invoked.set(invoked.get() + 1);
                    async { Ok(()) }.boxed_local()
                }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    assert_eq!(
        driver
            .run_until(host.get(contract, json!({}), GetOptions::default()))
            .err()
            .unwrap()
            .code,
        ErrorCode::Invalid
    );
    assert_eq!(drops.get(), 1);
    assert_eq!(calls.get(), 0);
    driver.run_until(host.close()).unwrap();
    assert_eq!(drops.get(), 1);
}

#[test]
fn refused_owner_destructor_can_reenter_host_acquisition() {
    struct Reenter {
        host: Host,
        contract: Rc<Contract>,
        acquired: Rc<RefCell<Option<Reference>>>,
    }
    impl Drop for Reenter {
        fn drop(&mut self) {
            let reference = self
                .host
                .hold(self.contract.clone(), json!({}), GetOptions::default())
                .unwrap();
            *self.acquired.borrow_mut() = Some(reference);
        }
    }
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("reentrant", json!(null)), driver.clone());
    let helper = Rc::new(Contract::local("helper", 1, json!({}), BTreeMap::new()).unwrap());
    host.serve(
        helper.clone(),
        Rc::new(|_, _, _| Ok(Implementation::new())),
        ServeOptions::default(),
    )
    .unwrap();
    let failed = Rc::new(
        Contract::local(
            "failed",
            1,
            json!({}),
            BTreeMap::from([(
                "missing".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            )]),
        )
        .unwrap(),
    );
    let saved = Rc::new(RefCell::new(None));
    let (from_drop, reentrant_host) = (saved.clone(), host.clone());
    host.serve(
        failed.clone(),
        Rc::new(move |_, _, _| {
            Ok(Implementation::new().retain_owner(Rc::new(Reenter {
                host: reentrant_host.clone(),
                contract: helper.clone(),
                acquired: from_drop.clone(),
            })))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let error = driver
        .run_until(host.get(failed, json!({}), GetOptions::default()))
        .err()
        .unwrap();
    assert_eq!(error.reason, "implementation");
    let acquired = saved.borrow_mut().take().unwrap();
    driver.run_until(acquired.ready()).unwrap();
    acquired.release();
    driver.run_until(host.close()).unwrap();
}

#[test]
fn retained_owner_outlives_last_callback_capture_destructor_across_clones() {
    struct Capture {
        owner: std::rc::Weak<Owner>,
        drops: Rc<Cell<u32>>,
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            assert!(self.owner.upgrade().is_some());
            self.drops.set(self.drops.get() + 1);
        }
    }
    let owner_drops = Rc::new(Cell::new(0));
    let capture_drops = Rc::new(Cell::new(0));
    let owner = Rc::new(Owner {
        drops: owner_drops.clone(),
    });
    let capture = Capture {
        owner: Rc::downgrade(&owner),
        drops: capture_drops.clone(),
    };
    let implementation =
        Implementation::new()
            .retain_owner(owner)
            .method("callback", move |(): (), _| {
                let _retained = &capture;
                async { Ok(()) }.boxed_local()
            });
    let selected = implementation.clone();
    drop(implementation);
    assert_eq!((owner_drops.get(), capture_drops.get()), (0, 0));
    drop(selected);
    assert_eq!((owner_drops.get(), capture_drops.get()), (1, 1));
}
