use futures::{FutureExt, channel::oneshot};
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

#[test]
fn ordinary_disposal_cancels_resource_open_signal_before_close_and_forgetting() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("resource-signal", json!(null)),
        driver.clone(),
    );
    let contract =
        Rc::new(Contract::local("resource-signal", 1, json!({}), BTreeMap::new()).unwrap());
    let signal = Rc::new(RefCell::new(None::<CancellationToken>));
    let observed = Rc::new(Cell::new(0));
    let closes = Rc::new(Cell::new(0));
    let close_saw_cancel = Rc::new(Cell::new(false));
    let (saved, watcher, closed, saw_cancel, tasks) = (
        signal.clone(),
        observed.clone(),
        closes.clone(),
        close_saw_cancel.clone(),
        driver.clone(),
    );
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let (saved, watcher, tasks) = (saved.clone(), watcher.clone(), tasks.clone());
            let (closed, saw_cancel, closing_signal) =
                (closed.clone(), saw_cancel.clone(), saved.clone());
            declaration.resource(
                "r",
                ResourceDeclaration::new(move |token| {
                    *saved.borrow_mut() = Some(token.clone());
                    let watcher = watcher.clone();
                    tasks.spawn(
                        async move {
                            token.cancelled().await;
                            watcher.set(watcher.get() + 1);
                        }
                        .boxed_local(),
                    );
                    async { Ok(7) }.boxed_local()
                })
                .with_close(move |value| {
                    assert_eq!(*value, 7);
                    saw_cancel.set(closing_signal.borrow().as_ref().unwrap().is_cancelled());
                    closed.set(closed.get() + 1);
                    async { Ok(()) }.boxed_local()
                }),
            )?;
            Ok(Implementation::new())
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    assert!(!signal.borrow().as_ref().unwrap().is_cancelled());
    reference.release();
    driver.run_until_stalled();
    assert_eq!(closes.get(), 1);
    assert!(
        close_saw_cancel.get(),
        "resource opener observes termination before closer runs"
    );
    assert_eq!(
        observed.get(),
        1,
        "signal waiter settles without a later Host.close"
    );
    driver.run_until(host.close()).unwrap();
    assert_eq!(observed.get(), 1);
    assert_eq!(closes.get(), 1);
    drop(reference);
}

#[test]
fn ordinary_release_preserves_active_context_and_physical_lease_before_resource_disposal() {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("active-signal", json!(null)),
        driver.clone(),
    );
    let contract = Rc::new(
        Contract::local(
            "active-signal",
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            )]),
        )
        .unwrap(),
    );
    let token = Rc::new(RefCell::new(None::<CancellationToken>));
    let saved = Rc::new(RefCell::new(None::<Context>));
    let lease = Rc::new(RefCell::new(None::<InvocationLease>));
    let closed = Rc::new(Cell::new(0));
    let (release, receive) = oneshot::channel();
    let receive = Rc::new(RefCell::new(Some(receive)));
    let (signal, context, physical, closes) =
        (token.clone(), saved.clone(), lease.clone(), closed.clone());
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            let signal = signal.clone();
            let closes = closes.clone();
            declaration.resource(
                "r",
                ResourceDeclaration::new(move |token| {
                    *signal.borrow_mut() = Some(token);
                    async { Ok(()) }.boxed_local()
                })
                .with_close(move |_| {
                    closes.set(closes.get() + 1);
                    async { Ok(()) }.boxed_local()
                }),
            )?;
            let owner = declaration.invocation_owner();
            let (context, physical, receive) = (context.clone(), physical.clone(), receive.clone());
            Ok(Implementation::new().method("run", move |(): (), ctx| {
                *physical.borrow_mut() = Some(owner.retain(&ctx).unwrap());
                *context.borrow_mut() = Some(ctx.clone());
                let receive = receive.borrow_mut().take().unwrap();
                async move {
                    receive.await.unwrap();
                    ctx.check(ctx.host_id())?;
                    Ok(())
                }
                .boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let reference = driver
        .run_until(host.get(contract, json!({}), GetOptions::default()))
        .unwrap();
    let mut call = reference.call::<(), ()>("run", (), None).boxed_local();
    assert!(call.as_mut().now_or_never().is_none());
    reference.release();
    driver.run_until_stalled();
    saved.borrow().as_ref().unwrap().check(host.id()).unwrap();
    assert!(!token.borrow().as_ref().unwrap().is_cancelled());
    assert_eq!(closed.get(), 0);
    release.send(()).unwrap();
    driver.run_until(call).unwrap();
    assert_eq!(
        saved
            .borrow()
            .as_ref()
            .unwrap()
            .check(host.id())
            .unwrap_err()
            .reason,
        "expired"
    );
    driver.run_until_stalled();
    assert_eq!(
        closed.get(),
        0,
        "detached physical ownership still prevents cleanup"
    );
    assert!(!token.borrow().as_ref().unwrap().is_cancelled());
    drop(lease.borrow_mut().take());
    driver.run_until_stalled();
    assert!(token.borrow().as_ref().unwrap().is_cancelled());
    assert_eq!(closed.get(), 1);
    driver.run_until(host.close()).unwrap();
    assert_eq!(closed.get(), 1);
}
