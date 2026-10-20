use futures::{
    FutureExt,
    channel::oneshot,
    future::{Either, select},
};
use serde_json::json;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;
fn token(name: &str, result: bool) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: None,
                    result: result.then(|| ValueSchema::Portable(json!({ "type": "integer" }))),
                },
            )]),
        )
        .unwrap(),
    )
}

fn run(test: impl FnOnce(Rc<TokioDriver>) -> futures::future::LocalBoxFuture<'static, ()>) {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(15), test(driver.clone()))
                .await
                .unwrap();
        }));
}

#[test]
fn dropped_waiter_late_success_or_throw_keeps_resources_until_physical_settlement() {
    for throws in [false, true] {
        run(move |driver| {
            async move {
                let host = Host::new(HostOptions::new("physical", json!("host")), driver.clone());
                let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
                let gate = token("gate", true);
                let sink = token("close", false);
                let source = token("source", true);
                let (started_tx, started_rx) = oneshot::channel();
                let (gate_tx, gate_rx) = oneshot::channel();
                let started = Rc::new(RefCell::new(Some(started_tx)));
                let gate_receive = Rc::new(RefCell::new(Some(gate_rx)));
                host.serve(
                    gate.clone(),
                    Rc::new(move |_, _, _| {
                        let started = started.clone();
                        let receive = gate_receive.clone();
                        Ok(Implementation::new().json_method("run", move |_, _| {
                            let send = started.borrow_mut().take().unwrap();
                            let receive = receive.borrow_mut().take().unwrap();
                            async move {
                                send.send(()).unwrap();
                                receive.await.unwrap();
                                Ok(Some(json!(7)))
                            }
                            .boxed_local()
                        }))
                    }),
                    ServeOptions::default(),
                )
                .unwrap();
                let closed = Rc::new(Cell::new(0));
                let counter = closed.clone();
                host.serve(
                    sink.clone(),
                    Rc::new(move |_, _, _| {
                        let counter = counter.clone();
                        Ok(Implementation::new().json_method("run", move |_, _| {
                            counter.set(counter.get() + 1);
                            async { Ok(None) }.boxed_local()
                        }))
                    }),
                    ServeOptions::default(),
                )
                .unwrap();
                let js = r#"
const source = __FIXTURE_0__, gate = __FIXTURE_1__, sink = __FIXTURE_2__;
export default {
    name: 'physical', uses: [], setup(scope, d) {
        d.serve(source, (input, self) => {
            const wait = self.get(gate, {}), close = self.get(sink, {});
            self.resource('managed', {
                open: async () => ({
                    privateValue: 17
                }), close: async (value) => {
                    if (value.privateValue !== 17)
                        throw Error('value');
                    await scope.root(context => close.run(context));
                }
            });
            return {
                methods: {
                    async run(context) {
                        await wait.run(context);
                        __FIXTURE_3__;
                        context.check();
                        return 8;
                    }
                }, state: {}
            };
        });
    }
};
"#
                .replace(
                    "__FIXTURE_0__",
                    &(serde_json::to_string(source.descriptor()).unwrap()),
                )
                .replace(
                    "__FIXTURE_1__",
                    &(serde_json::to_string(gate.descriptor()).unwrap()),
                )
                .replace(
                    "__FIXTURE_2__",
                    &(serde_json::to_string(sink.descriptor()).unwrap()),
                )
                .replace(
                    "__FIXTURE_3__",
                    if throws {
                        "throw Error('late throw')"
                    } else {
                        ""
                    },
                );
                let artifact =
                    BundledJsArtifact::new("physical", js, vec!["default".into()]).unwrap();
                let facet = executor
                    .prepare(host.id(), artifact, "default".into())
                    .await
                    .unwrap();
                host.install_executable(facet, LoadOptions::default())
                    .unwrap()
                    .await
                    .unwrap();
                let reference = host
                    .get(source, json!({}), GetOptions::default())
                    .await
                    .unwrap();
                let call = reference.call_json("run", None, None).boxed_local();
                let Either::Right((Ok(()), call)) = select(call, started_rx).await else {
                    panic!("call settled before native gate")
                };
                drop(call);
                reference.release();
                let mut close = host.close().boxed_local();
                assert!(futures::poll!(&mut close).is_pending());
                assert_eq!(closed.get(), 0);
                assert!(executor.stats().physical_jobs > 0);
                gate_tx.send(()).unwrap();
                close.await.unwrap();
                assert_eq!(closed.get(), 1);
                assert_eq!(executor.stats().physical_jobs, 0);
                assert_eq!(executor.stats().contexts, 0);
            }
            .boxed_local()
        });
    }
}

#[test]
fn detached_native_operation_retains_entered_javascript_owner_after_top_promise_settles() {
    run(|driver| check_detached_operation(driver).boxed_local());
}

async fn check_detached_operation(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("detached", json!("host")), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let gate = token("gate", true);
    let source = token("source", true);
    let (start_tx, start_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let send = Rc::new(RefCell::new(Some(start_tx)));
    let receive = Rc::new(RefCell::new(Some(release_rx)));
    host.serve(
        gate.clone(),
        Rc::new(move |_, _, _| {
            let send = send.clone();
            let receive = receive.clone();
            Ok(Implementation::new().json_method("run", move |_, _| {
                let send = send.borrow_mut().take().unwrap();
                let receive = receive.borrow_mut().take().unwrap();
                async move {
                    send.send(()).unwrap();
                    receive.await.unwrap();
                    Ok(Some(json!(1)))
                }
                .boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let closed = Rc::new(Cell::new(0));
    let close_counter = closed.clone();
    let sink = token("sink", false);
    host.serve(
        sink.clone(),
        Rc::new(move |_, _, _| {
            let counter = close_counter.clone();
            Ok(Implementation::new().json_method("run", move |_, _| {
                counter.set(counter.get() + 1);
                async { Ok(None) }.boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let js = r#"
const source = __FIXTURE_0__, gate = __FIXTURE_1__, sink = __FIXTURE_2__;
export default {
    name: 'detached', uses: [], setup(scope, d) {
        d.serve(source, (input, self) => {
            const dependency = self.get(gate, {}), closer = self.get(sink, {});
            self.resource('r', {
                open: () => 1, close: () => scope.root(ctx => closer.run(ctx))
            });
            return {
                methods: {
                    async run(ctx) {
                        dependency.run(ctx).catch(() => {
                        });
                        return 1;
                    }
                }, state: {}
            };
        });
    }
};
"#
    .replace(
        "__FIXTURE_0__",
        &(serde_json::to_string(source.descriptor()).unwrap()),
    )
    .replace(
        "__FIXTURE_1__",
        &(serde_json::to_string(gate.descriptor()).unwrap()),
    )
    .replace(
        "__FIXTURE_2__",
        &(serde_json::to_string(sink.descriptor()).unwrap()),
    );
    let facet = executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("detached", js, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();
    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(source, json!({}), GetOptions::default())
        .await
        .unwrap();
    assert_eq!(
        reference.call_json("run", None, None).await.unwrap(),
        Some(json!(1))
    );
    start_rx.await.unwrap();
    reference.release();
    let mut close = host.close().boxed_local();
    assert!(futures::poll!(&mut close).is_pending());
    assert_eq!(closed.get(), 0);
    release_tx.send(()).unwrap();
    close.await.unwrap();
    assert_eq!(closed.get(), 1);
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().contexts, 0);
}
