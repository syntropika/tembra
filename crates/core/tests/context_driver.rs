use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use futures::future::LocalBoxFuture;
use futures::{FutureExt, channel::oneshot};
use serde_json::json;
use tembra_contracts::{Contract, Member, ValueSchema};
use tembra_core::context::CancellationToken;
use tembra_core::driver::{Driver, ManualDriver};
use tembra_core::error::{ErrorCode, RuntimeError};
use tembra_core::{
    GetOptions, Host, HostOptions, Implementation, RefStatus, Reference, ResourceDeclaration,
    ServeOptions,
};

#[test]
fn timers_use_manual_monotonic_time_and_deadline_order() {
    let driver = ManualDriver::new();
    let events = Rc::new(RefCell::new(Vec::new()));
    for (milliseconds, label) in [(20, "later"), (10, "first")] {
        let wait = driver.sleep(Duration::from_millis(milliseconds));
        let events = events.clone();
        driver.spawn(
            async move {
                wait.await;
                events.borrow_mut().push(label);
            }
            .boxed_local(),
        );
    }
    driver.run_until_stalled();
    assert!(events.borrow().is_empty());
    driver.advance(Duration::from_millis(10));
    driver.run_until_stalled();
    assert_eq!(&*events.borrow(), &["first"]);
    driver.advance(Duration::from_millis(10));
    driver.run_until_stalled();
    assert_eq!(&*events.borrow(), &["first", "later"]);
    assert_eq!(driver.now(), Duration::from_millis(20));
}

#[test]
fn run_until_polls_runnable_work_before_advancing_timers() {
    let driver = ManualDriver::new();
    let (send, receive) = oneshot::channel();
    driver.spawn(
        async move {
            send.send(42).unwrap();
        }
        .boxed_local(),
    );
    let output = driver.run_until(async {
        let timeout = driver.sleep(Duration::from_secs(60));
        futures::pin_mut!(timeout);
        match futures::future::select(receive, timeout).await {
            futures::future::Either::Left((result, _)) => result.unwrap(),
            futures::future::Either::Right(_) => panic!("work should finish before timeout"),
        }
    });
    assert_eq!(output, 42);
    assert_eq!(driver.now(), Duration::ZERO);
    driver.run_until(driver.sleep(Duration::from_secs(2)));
    assert_eq!(driver.now(), Duration::from_secs(2));
}

#[test]
fn callbacks_can_spawn_without_reborrowing_the_running_pool() {
    let driver = ManualDriver::new();
    let count = Rc::new(Cell::new(0));
    let nested_driver = driver.clone();
    let count_in_task = count.clone();
    driver.spawn(
        async move {
            count_in_task.set(1);
            nested_driver.spawn(
                async move {
                    count_in_task.set(2);
                }
                .boxed_local(),
            );
        }
        .boxed_local(),
    );
    driver.run_until_stalled();
    assert_eq!(count.get(), 2);
}

#[test]
fn cancellation_wakes_every_waiter_and_child_cancel_is_isolated() {
    let driver = ManualDriver::new();
    let root = CancellationToken::new();
    let child = root.child();
    let sibling = root.child();
    let reason = RuntimeError::aborted("requested", "stop requested");
    let received = Rc::new(RefCell::new(Vec::new()));
    for token in [child.clone(), child.clone()] {
        let received = received.clone();
        driver.spawn(
            async move {
                let reason = token.cancelled().await;
                received.borrow_mut().push(reason);
            }
            .boxed_local(),
        );
    }
    driver.run_until_stalled();
    child.cancel(reason.clone());
    driver.run_until_stalled();
    assert_eq!(&*received.borrow(), &[reason.clone(), reason]);
    assert!(!root.is_cancelled());
    assert!(!sibling.is_cancelled());
    root.cancel("root-stop");
    assert!(sibling.is_cancelled());
    assert_eq!(child.check().unwrap_err().reason, "requested");
}

#[test]
fn structured_errors_keep_categories_details_and_retry_policy() {
    let mut error = RuntimeError::invalid("input", "missing object key");
    error.set_details(serde_json::json!({"path": "/id"}));
    assert_eq!(error.to_string(), "invalid/input: missing object key");
    assert_eq!(error.clone().details()["path"], "/id");
    assert!(!error.retryable());
    assert!(RuntimeError::new(ErrorCode::Superseded, "retired", "old version").retryable());
    assert!(ErrorCode::UpgradeConflict.retryable());
    assert_eq!(
        serde_json::to_string(&ErrorCode::NotInVariant).unwrap(),
        "\"not_in_variant\""
    );
}

struct SlowObject {
    host: Host,
    driver: Rc<ManualDriver>,
    contract: Rc<Contract>,
    open: oneshot::Sender<()>,
    factories: Rc<Cell<usize>>,
    closed: Rc<Cell<usize>>,
    calls: Rc<Cell<usize>>,
}

fn slow_object(name: &str) -> SlowObject {
    let driver = ManualDriver::new();
    let host = Host::new(
        HostOptions::new("context-tests", json!({"kind":"host"})),
        driver.clone(),
    );
    let contract = Rc::new(
        Contract::local(
            name,
            1,
            json!({"type":"object", "properties":{}}),
            std::collections::BTreeMap::from([(
                "read".to_owned(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Native("context-tests.number@1".to_owned())),
                },
            )]),
        )
        .unwrap(),
    );
    let (open, opening) = oneshot::channel();
    let opening = Rc::new(RefCell::new(Some(opening)));
    let factories = Rc::new(Cell::new(0));
    let factory_count = factories.clone();
    let closed = Rc::new(Cell::new(0));
    let close_count = closed.clone();
    let calls = Rc::new(Cell::new(0));
    let call_count = calls.clone();
    host.serve(
        contract.clone(),
        Rc::new(move |_, _, declaration| {
            factory_count.set(factory_count.get() + 1);
            let opening = opening.clone();
            let close_count = close_count.clone();
            let resource = declaration.resource(
                "delayed",
                ResourceDeclaration::new(move |_| {
                    let opening = opening.borrow_mut().take();
                    async move {
                        if let Some(opening) = opening {
                            opening.await.map_err(|_| {
                                RuntimeError::unavailable(
                                    "fixture",
                                    "resource opening sender dropped",
                                )
                            })?;
                        }
                        Ok(7_u64)
                    }
                    .boxed_local()
                })
                .with_close(move |_| {
                    let close_count = close_count.clone();
                    async move {
                        close_count.set(close_count.get() + 1);
                        Ok(())
                    }
                    .boxed_local()
                }),
            )?;
            let call_count = call_count.clone();
            Ok(
                Implementation::new().method::<(), u64>("read", move |(), _| {
                    let resource = resource.clone();
                    let call_count = call_count.clone();
                    async move {
                        call_count.set(call_count.get() + 1);
                        Ok(*resource.value()?)
                    }
                    .boxed_local()
                }),
            )
        }),
        ServeOptions::default(),
    )
    .unwrap();
    SlowObject {
        host,
        driver,
        contract,
        open,
        factories,
        closed,
        calls,
    }
}

#[test]
fn a_polled_ctx_get_expires_with_its_root_and_releases_only_its_claim() {
    let fixture = slow_object("context-tests.expired-get");
    let external = fixture
        .host
        .hold(fixture.contract.clone(), json!({}), GetOptions::default())
        .unwrap();
    let pending: Rc<RefCell<Option<LocalBoxFuture<'static, tembra_core::Result<Reference>>>>> =
        Rc::default();
    let retained = pending.clone();
    let contract = fixture.contract.clone();
    fixture
        .driver
        .run_until(fixture.host.root(move |ctx| {
            async move {
                let mut get =
                    async move { ctx.get(contract, json!({}), GetOptions::default()).await }
                        .boxed_local();
                assert!(
                    futures::poll!(get.as_mut()).is_pending(),
                    "acquisition must be polled inside the live root"
                );
                *retained.borrow_mut() = Some(get);
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    let get = pending.borrow_mut().take().unwrap();
    let error = fixture
        .driver
        .run_until(get)
        .err()
        .expect("settled root must reject its pending get");
    assert_eq!(error.code, ErrorCode::Aborted);
    assert_eq!(error.reason, "expired");
    assert_eq!(external.status(), RefStatus::Loading);
    fixture.open.send(()).unwrap();
    fixture.driver.run_until(external.ready()).unwrap();
    assert_eq!(external.status(), RefStatus::Live);
    assert_eq!(fixture.factories.get(), 1);
    assert_eq!(fixture.closed.get(), 0);
    external.release();
    fixture.driver.run_until_stalled();
    assert_eq!(
        fixture.closed.get(),
        1,
        "expired get must not retain a hidden claim"
    );
    fixture.driver.run_until(fixture.host.close()).unwrap();
}

#[test]
fn a_loading_reference_call_wakes_on_scope_expiry_without_releasing_external_claim() {
    let fixture = slow_object("context-tests.expired-call");
    let external = fixture
        .host
        .hold(fixture.contract.clone(), json!({}), GetOptions::default())
        .unwrap();
    let pending: Rc<RefCell<Option<LocalBoxFuture<'static, tembra_core::Result<u64>>>>> =
        Rc::default();
    let retained = pending.clone();
    let target = external.clone();
    fixture
        .driver
        .run_until(fixture.host.root(move |ctx| {
            async move {
                let mut call = async move { target.call::<(), u64>("read", (), Some(&ctx)).await }
                    .boxed_local();
                assert!(
                    futures::poll!(call.as_mut()).is_pending(),
                    "call must wait while its context is live"
                );
                *retained.borrow_mut() = Some(call);
                Ok(())
            }
            .boxed_local()
        }))
        .unwrap();
    let call = pending.borrow_mut().take().unwrap();
    let error = fixture
        .driver
        .run_until(call)
        .expect_err("settlement must wake a loading call");
    assert_eq!(error.code, ErrorCode::Aborted);
    assert_eq!(error.reason, "expired");
    assert_eq!(external.status(), RefStatus::Loading);
    assert_eq!(
        fixture.calls.get(),
        0,
        "expired invocation must never reach the method"
    );
    fixture.open.send(()).unwrap();
    assert_eq!(
        fixture
            .driver
            .run_until(external.call::<(), u64>("read", (), None))
            .unwrap(),
        7
    );
    assert_eq!(fixture.factories.get(), 1);
    assert_eq!(fixture.calls.get(), 1);
    assert_eq!(fixture.closed.get(), 0);
    external.release();
    fixture.driver.run_until_stalled();
    assert_eq!(fixture.closed.get(), 1);
    fixture.driver.run_until(fixture.host.close()).unwrap();
}

#[test]
fn cancelling_one_concurrent_ctx_get_preserves_the_other_live_claim() {
    let fixture = slow_object("context-tests.independent-get-cancellation");
    let cancellation = Rc::new(RefCell::new(None));
    let first_context = cancellation.clone();
    let first_host = fixture.host.clone();
    let first_contract = fixture.contract.clone();
    let (first_done, first_finished) = oneshot::channel();
    fixture.driver.spawn(
        async move {
            let result = first_host
                .root(move |ctx| {
                    *first_context.borrow_mut() = Some(ctx.cancellation().clone());
                    async move {
                        ctx.get(first_contract, json!({}), GetOptions::default())
                            .await
                            .map(|_| ())
                    }
                    .boxed_local()
                })
                .await;
            let _ = first_done.send(result);
        }
        .boxed_local(),
    );

    let second_host = fixture.host.clone();
    let second_contract = fixture.contract.clone();
    let (second_ready, second_is_ready) = oneshot::channel();
    let (finish_second, continue_second) = oneshot::channel();
    let (second_done, second_finished) = oneshot::channel();
    fixture.driver.spawn(
        async move {
            let result = second_host
                .root(move |ctx| {
                    async move {
                        let reference = ctx
                            .get(second_contract, json!({}), GetOptions::default())
                            .await?;
                        let _ = second_ready.send((reference.status(), reference.object_id()));
                        continue_second.await.map_err(|_| {
                            RuntimeError::unavailable(
                                "fixture",
                                "second root completion sender dropped",
                            )
                        })?;
                        reference.call::<(), u64>("read", (), Some(&ctx)).await
                    }
                    .boxed_local()
                })
                .await;
            let _ = second_done.send(result);
        }
        .boxed_local(),
    );

    fixture.driver.run_until_stalled();
    assert_eq!(fixture.factories.get(), 1);
    assert_eq!(fixture.closed.get(), 0);
    cancellation
        .borrow()
        .as_ref()
        .unwrap()
        .cancel("first-caller-left");
    let first_error = fixture
        .driver
        .run_until(first_finished)
        .unwrap()
        .expect_err("cancelled get must fail");
    assert_eq!(first_error.code, ErrorCode::Aborted);
    assert_eq!(first_error.reason, "first-caller-left");
    assert_eq!(
        fixture.closed.get(),
        0,
        "another pending get keeps activation owned"
    );
    fixture.open.send(()).unwrap();
    let (status, _) = fixture.driver.run_until(second_is_ready).unwrap();
    assert_eq!(status, RefStatus::Live);
    assert_eq!(
        fixture.factories.get(),
        1,
        "cancelling one get must not restart shared activation"
    );
    assert_eq!(fixture.closed.get(), 0);
    finish_second.send(()).unwrap();
    assert_eq!(
        fixture.driver.run_until(second_finished).unwrap().unwrap(),
        7
    );
    fixture.driver.run_until_stalled();
    assert_eq!(fixture.closed.get(), 1);
    fixture.driver.run_until(fixture.host.close()).unwrap();
}
