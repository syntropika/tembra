use futures::{FutureExt, task::LocalSpawnExt};
use std::time::Duration;
use tembra::{Host, Orb, Service};
use tembra_executor_js::{CooperativeExecutor, Runtime};

#[test]
fn managed_native_work_progresses_without_an_active_v8_evaluation() {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&tokio, async {
        let executor = CooperativeExecutor::new();
        let (send, receive) = futures::channel::oneshot::channel();
        executor
            .spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                send.send(42).unwrap();
            })
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), receive)
                .await
                .expect("native work stopped while V8 was idle")
                .unwrap(),
            42
        );
    });
}

#[test]
fn native_calls_can_spawn_local_children_before_and_after_suspension() {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&tokio,async {
        let driver = CooperativeExecutor::new();
        let native = Host::new(driver.clone());
        native.install(vec![Orb::new("native",1).service(Service::new("native",|_|Ok(())).method("read",|_,_,_:Vec<serde_json::Value>|async move {
            let first=tokio::task::spawn_local(async {tokio::task::yield_now().await;20}).await.unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
            let second=tokio::task::spawn_local(async {22}).await.unwrap();
            Ok(first+second)
        }))]).unwrap();
        let mut runtime=Runtime::with_driver(native,include_str!("../../../packages/ts/sdk/dist/embedded.js"),driver).unwrap();
        let result=tokio::time::timeout(Duration::from_secs(2),runtime.evaluate("(async()=>{const ref=await host.get('native','main');try{return await ref.read();}finally{ref.release();}})()")).await.unwrap().unwrap();
        assert_eq!(result,serde_json::json!(42));runtime.close().await.unwrap();
    }.boxed_local());
}

#[test]
fn self_waking_work_yields_to_other_local_tasks() {
    use std::{cell::Cell, rc::Rc, task::Poll};
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&tokio, async {
        let executor = CooperativeExecutor::new();
        let polls = Rc::new(Cell::new(0));
        let observed = polls.clone();
        let (send, receive) = futures::channel::oneshot::channel();
        let (started, ready) = futures::channel::oneshot::channel();
        executor
            .spawn_local(async move {
                let mut started = Some(started);
                std::future::poll_fn(|cx| {
                    if let Some(started) = started.take() {
                        started.send(()).unwrap();
                    }
                    let count = observed.get() + 1;
                    observed.set(count);
                    if count == 100_000 {
                        std::task::Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                send.send(()).unwrap();
            })
            .unwrap();
        ready.await.unwrap();
        assert!(
            polls.get() < 100_000,
            "the driver starved the observing task"
        );
        receive.await.unwrap();
    });
}

#[test]
fn executor_shutdown_rejects_new_work_instead_of_retaining_an_idle_cycle() {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let executor = local.block_on(&tokio, async { CooperativeExecutor::new() });
    drop(local);
    assert!(executor.spawn_local(async {}).is_err());
}

#[test]
fn runtime_polling_does_not_starve_other_local_tasks() {
    use std::{cell::Cell, rc::Rc, task::Poll};
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&tokio, async {
        let driver = CooperativeExecutor::new();
        let native = Host::new(driver.clone());
        let release = Rc::new(Cell::new(false));
        let observed = release.clone();
        let (started, ready) = futures::channel::oneshot::channel();
        let started = Rc::new(std::cell::RefCell::new(Some(started)));
        native.install(vec![Orb::new("fair", 1).service(
            Service::new("fair", move |_| Ok((observed.clone(), started.clone()))).method("wait", |state, _, _: Vec<serde_json::Value>| async move {
                state.1.borrow_mut().take().unwrap().send(()).unwrap();
                std::future::poll_fn(|cx| {
                    if state.0.get() { Poll::Ready(()) }
                    else { cx.waker().wake_by_ref(); Poll::Pending }
                }).await;
                Ok(42)
            })
        )]).unwrap();
        let release_task = tokio::task::spawn_local(async move {
            ready.await.unwrap();
            tokio::task::yield_now().await;
            release.set(true);
        });
        let mut runtime = Runtime::with_driver(native, include_str!("../../../packages/ts/sdk/dist/embedded.js"), driver).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), runtime.evaluate("(async()=>{const ref=await host.get('fair','main');try{return await ref.wait();}finally{ref.release();}})()")).await.unwrap().unwrap();
        assert_eq!(result, serde_json::json!(42));
        release_task.await.unwrap();
        runtime.close().await.unwrap();
    });
}

#[test]
fn legacy_executor_value_can_be_constructed_before_entering_a_local_set() {
    let executor = tembra_executor_js::TokioExecutor;
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&tokio, async {
        let native = Host::new(executor);
        native
            .install(vec![
                Orb::new("legacy", 1).service(
                    Service::new("legacy", |_| Ok(()))
                        .method("read", |_, _, _: Vec<serde_json::Value>| async { Ok(42) }),
                ),
            ])
            .unwrap();
        let mut runtime = Runtime::new(
            native,
            include_str!("../../../packages/ts/sdk/dist/embedded.js"),
        )
        .unwrap();
        assert_eq!(
            runtime
                .call("legacy", &["main".into()], "read", serde_json::json!([]))
                .await
                .unwrap(),
            serde_json::json!(42)
        );
        runtime.close().await.unwrap();
    });
}

#[test]
fn a_panicking_spawned_future_does_not_discard_other_executor_work() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio, async {
            let executor: Box<dyn futures::task::LocalSpawn> = if cooperative {
                Box::new(CooperativeExecutor::new())
            } else {
                Box::new(tembra_executor_js::TokioExecutor::new())
            };
            let (send, receive) = futures::channel::oneshot::channel();
            executor
                .spawn_local(async { panic!("unmanaged task panic") })
                .unwrap();
            executor
                .spawn_local(async {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    send.send(42).unwrap();
                })
                .unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), receive)
                    .await
                    .unwrap()
                    .unwrap(),
                42
            );
        });
    }
}
