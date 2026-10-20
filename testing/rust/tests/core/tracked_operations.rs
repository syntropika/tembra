use futures::{
    FutureExt,
    task::{ArcWake, waker},
};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::Context as TaskContext,
};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
fn contract(name: &str) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: Some(ValueSchema::Native("marker@1".into())),
                    result: Some(ValueSchema::Native("marker@1".into())),
                },
            )]),
        )
        .unwrap(),
    )
}
thread_local! {static RECEIPT:RefCell<Option<InvocationReceipt>>=const {RefCell::new(None)};}
struct Reentrant(AtomicUsize);
impl ArcWake for Reentrant {
    fn wake_by_ref(this: &Arc<Self>) {
        this.0.fetch_add(1, Ordering::Relaxed);
        RECEIPT.with(|receipt| {
            assert!(
                receipt
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .settled()
                    .now_or_never()
                    .is_some()
            )
        });
    }
}
#[test]
fn tracked_entry_preserves_identity_scope_and_last_physical_release_outside_borrows() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("tracked", json!(null)), driver.clone());
    let contract = contract("tracked");
    let lease = Rc::new(RefCell::new(None));
    let context = Rc::new(RefCell::new(None));
    let closed = Rc::new(Cell::new(0));
    let (physical, saved, closes) = (lease.clone(), context.clone(), closed.clone());
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let owner = declaration.invocation_owner();
            let (physical, saved) = (physical.clone(), saved.clone());
            let closes = closes.clone();
            declaration.resource(
                "r",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()).with_close(
                    move |_| {
                        closes.set(closes.get() + 1);
                        async { Ok(()) }.boxed_local()
                    },
                ),
            )?;
            Ok(
                Implementation::new().method("run", move |value: Rc<u32>, ctx| {
                    *physical.borrow_mut() = Some(owner.retain(&ctx).unwrap());
                    *saved.borrow_mut() = Some(ctx);
                    async move { Ok(value) }.boxed_local()
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    let value = Rc::new(7_u32);
    let call = reference.call_tracked::<Rc<u32>, Rc<u32>>("run", value.clone(), None);
    let receipt = call.receipt();
    let result = driver.run_until(call).unwrap();
    assert!(Rc::ptr_eq(&value, &result));
    let context = context.borrow().clone().unwrap();
    assert!(receipt.matches_context(&context));
    assert_eq!(context.check(host.id()).unwrap_err().reason, "expired");
    RECEIPT.with(|stored| *stored.borrow_mut() = Some(receipt.clone()));
    let observer = Arc::new(Reentrant(AtomicUsize::new(0)));
    let wake = waker(observer.clone());
    let mut task = TaskContext::from_waker(&wake);
    let mut wait = receipt.settled();
    assert!(wait.as_mut().poll(&mut task).is_pending());
    reference.release();
    let mut closing = host.close().boxed_local();
    assert!(closing.as_mut().now_or_never().is_none());
    assert_eq!(closed.get(), 0);
    drop(lease.borrow_mut().take());
    driver.run_until(closing).unwrap();
    assert!(receipt.settled().now_or_never().is_some());
    assert!(!receipt.matches_context(&context));
    assert_eq!(closed.get(), 1);
    assert_eq!(observer.0.load(Ordering::Relaxed), 1);
    RECEIPT.with(|stored| {
        stored.borrow_mut().take();
    });
}
#[test]
fn unpolled_and_refused_tracked_calls_complete_without_entry() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("unentered", json!(null)), driver.clone());
    let contract = contract("unentered");
    let calls = Rc::new(Cell::new(0));
    let count = calls.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, _| {
            let count = count.clone();
            Ok(
                Implementation::new().method("run", move |value: Rc<u32>, _| {
                    count.set(count.get() + 1);
                    async move { Ok(value) }.boxed_local()
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    let call = reference.call_tracked::<Rc<u32>, Rc<u32>>("run", Rc::new(1), None);
    let receipt = call.receipt();
    assert!(receipt.settled().now_or_never().is_none());
    drop(call);
    assert!(receipt.settled().now_or_never().is_some());
    let call = reference.call_tracked::<u8, Rc<u32>>("run", 1, None);
    let receipt = call.receipt();
    assert_eq!(driver.run_until(call).unwrap_err().reason, "native_type");
    assert!(receipt.settled().now_or_never().is_some());
    assert_eq!(calls.get(), 0);
    reference.release();
    driver.run_until(host.close()).unwrap();
}
#[test]
fn actual_native_preflight_rejects_wrong_types_and_json_only_channel_without_effects() {
    let driver = ManualDriver::new();
    let host = Host::new(HostOptions::new("preflight", json!(null)), driver.clone());
    let registry = Registry::new("ops", ConflictPolicy::Error).unwrap();
    let view = host.registry(&registry).unwrap();
    let count = Rc::new(Cell::new(0));
    for portable in [false, true] {
        let token = Rc::new(
            Contract::local(
                if portable { "json" } else { "native" },
                1,
                json!({}),
                BTreeMap::from([(
                    "run".into(),
                    Member::Method {
                        args: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                        result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                    },
                )]),
            )
            .unwrap(),
        );
        let registry = registry.clone();
        let count = count.clone();
        host.serve(
            token,
            Rc::new(move |_, _, declaration| {
                declaration.register_method(
                    &registry,
                    if portable { "json" } else { "native" },
                    json!(null),
                    "run",
                )?;
                let count = count.clone();
                Ok(if portable {
                    Implementation::new().json_method("run", move |_, _| {
                        count.set(count.get() + 1);
                        async { Ok(Some(json!(1))) }.boxed_local()
                    })
                } else {
                    Implementation::new().method("run", move |value: i32, _| {
                        count.set(count.get() + 1);
                        async move { Ok(value) }.boxed_local()
                    })
                })
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
        .unwrap();
    }
    driver.run_until(host.ready()).unwrap();
    let snapshot = view.table(None).unwrap();
    let native = snapshot.get("native").unwrap().operation().unwrap().clone();
    let json = snapshot.get("json").unwrap().operation().unwrap().clone();
    driver
        .run_until(host.root(move |ctx| {
            async move {
                native.check_native::<i32, i32>(&ctx)?;
                assert_eq!(
                    native.check_native::<u8, i32>(&ctx).unwrap_err().reason,
                    "native_type"
                );
                assert_eq!(
                    json.check_native::<i32, i32>(&ctx).unwrap_err().reason,
                    "native_type"
                );
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    assert_eq!(count.get(), 0);
    driver.run_until(host.close()).unwrap();
}
