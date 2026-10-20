//! Real Host reload acceptance for native executable generations and kept resources.
use futures::{
    FutureExt,
    channel::oneshot,
    future::{Either, LocalBoxFuture, select},
};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;

fn run(test: impl FnOnce(Rc<TokioDriver>) -> LocalBoxFuture<'static, ()>) {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(20), test(driver.clone()))
                .await
                .unwrap();
        }));
}

struct Environment {
    host: Host,
    executor: Executor,
    service: Rc<Contract>,
    monitor: Rc<Contract>,
    events: Rc<RefCell<Vec<Value>>>,
}
impl Environment {
    fn new(
        driver: Rc<TokioDriver>,
        gate: Option<oneshot::Receiver<()>>,
        options: ExecutorOptions,
    ) -> Self {
        let host = Host::new(HostOptions::new("reload", json!("host")), driver.clone());
        let executor = Executor::new(driver, options).unwrap();
        let service = Rc::new(
            Contract::local(
                "service",
                1,
                json!({}),
                BTreeMap::from([(
                    "read".into(),
                    Member::Method {
                        args: None,
                        result: Some(ValueSchema::Portable(json!({"type":"array","items":{}}))),
                    },
                )]),
            )
            .unwrap(),
        );
        let monitor = Rc::new(
            Contract::local(
                "monitor",
                1,
                json!({}),
                BTreeMap::from([
                    (
                        "record".into(),
                        Member::Method {
                            args: Some(ValueSchema::Portable(json!({"type":"array","items":{}}))),
                            result: None,
                        },
                    ),
                    (
                        "wait".into(),
                        Member::Method {
                            args: None,
                            result: None,
                        },
                    ),
                ]),
            )
            .unwrap(),
        );
        let events = Rc::new(RefCell::new(Vec::new()));
        let recorded = events.clone();
        let gate = Rc::new(RefCell::new(gate));
        host.serve(
            monitor.clone(),
            Rc::new(move |_, _, _| {
                let recorded = recorded.clone();
                let gate = gate.clone();
                Ok(Implementation::new()
                    .json_method("record", move |value, _| {
                        recorded.borrow_mut().push(value.unwrap());
                        async { Ok(None) }.boxed_local()
                    })
                    .json_method("wait", move |_, _| {
                        let gate = gate.borrow_mut().take().unwrap();
                        async move {
                            gate.await.unwrap();
                            Ok(None)
                        }
                        .boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
        Self {
            host,
            executor,
            service,
            monitor,
            events,
        }
    }
    fn source(&self, version: u64, resource_version: u64, fails: bool, close_gate: bool) -> String {
        format!(
            r#"
const token = {token}, monitor = {monitor};
export default {{
    name: 'resource', uses: [monitor],
    setup(scope, declare) {{
        declare.serve(token, (input, self) => {{
            const log = self.get(monitor, {{}});
            const resource = self.resource('kept', {{
                version: {resource_version}, deps: [log],
                async open() {{
                    await scope.root(ctx => log.record(['open', {version}], ctx));
                    const value = {{origin: {version}, read() {{return 40 + this.origin;}}}};
                    globalThis.resourceIdentity = value;
                    return value;
                }},
                async close(value) {{
                    await scope.root(ctx => log.record(['close', {version}, value.read()], ctx));
                    if ({close_gate}) await scope.root(ctx => log.wait(ctx));
                }}
            }});
            try {{
                resource.value;
                throw Error('factory read accepted');
            }} catch (error) {{
                if (error.reason !== 'factory') throw error;
            }}
            if ({fails}) throw Error('candidate refused');
            return {{methods: {{
                read(ctx) {{
                    const value = resource.value;
                    return [{version}, value.origin, value.read(), ctx.codeVersion,
                        value === globalThis.resourceIdentity];
                }}
            }}, state: {{}}}};
        }}, {{lifetime: 'host'}});
    }}
}};
"#,
            token = serde_json::to_string(self.service.descriptor()).unwrap(),
            monitor = serde_json::to_string(self.monitor.descriptor()).unwrap()
        )
    }
    async fn prepare(&self, source: String) -> ExecutableFacet {
        self.executor
            .prepare(
                self.host.id(),
                BundledJsArtifact::new("reload", source, vec!["default".into()]).unwrap(),
                "default".into(),
            )
            .await
            .unwrap()
    }
    async fn install(&self, version: u64) {
        let facet = self.prepare(self.source(version, 1, false, false)).await;
        self.host
            .install_executable(facet, LoadOptions::default())
            .unwrap()
            .await
            .unwrap();
    }
    async fn reference(&self) -> Reference {
        self.host
            .get(self.service.clone(), json!({}), GetOptions::default())
            .await
            .unwrap()
    }
    async fn reload(&self, candidate: ExecutableFacet) -> ReloadReport {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.host.reload(vec![candidate], ReloadOptions::default()),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "reload stalled events={:?} host={:?} executor={:?}",
                self.events.borrow(),
                self.host.errors(),
                self.executor.take_errors()
            )
        })
        .unwrap()
    }
    async fn close(&self) {
        tokio::time::timeout(Duration::from_secs(2), self.host.close())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "close stalled events={:?} host={:?} executor={:?} stats={:?}",
                    self.events.borrow(),
                    self.host.errors(),
                    self.executor.take_errors(),
                    self.executor.stats()
                )
            })
            .unwrap();
    }
    fn assert_closed(&self) {
        let stats = self.executor.stats();
        assert_eq!(stats.physical_jobs, 0);
        assert_eq!(stats.queued_jobs, 0);
        assert_eq!(stats.contexts, 0);
        assert_eq!(stats.callbacks, 0);
        assert_eq!(stats.capabilities, 0);
    }
}

#[test]
fn same_engine_resource_keep_uses_original_heap_value_and_new_closer_through_actual_settlement() {
    run(|driver| {
        async move {
            let (release, wait) = oneshot::channel();
            let environment = Environment::new(driver, Some(wait), ExecutorOptions::default());
            environment.install(1).await;
            let reference = environment.reference().await;
            assert_eq!(
                reference.call_json("read", None, None).await.unwrap(),
                Some(json!([1, 1, 41, 1, true]))
            );
            let candidate = environment
                .prepare(environment.source(2, 1, false, true))
                .await;
            let report = environment.reload(candidate).await;
            assert_eq!(report.current_version(), 2);
            assert_eq!(
                reference.call_json("read", None, None).await.unwrap(),
                Some(json!([2, 1, 41, 2, true]))
            );
            assert_eq!(*environment.events.borrow(), vec![json!(["open", 1])]);
            reference.release();
            let close = environment.host.close().boxed_local();
            let Either::Right(((), close)) = select(
                close,
                tokio::time::sleep(Duration::from_millis(10)).boxed_local(),
            )
            .await
            else {
                panic!("new closer must wait for its actual promise");
            };
            assert_eq!(
                *environment.events.borrow(),
                vec![json!(["open", 1]), json!(["close", 2, 41])]
            );
            assert!(environment.executor.stats().physical_jobs > 0);
            drop(close);
            release.send(()).unwrap();
            environment.close().await;
            environment.assert_closed();
        }
        .boxed_local()
    });
}

#[test]
fn failed_candidate_preserves_old_resource_closer_and_installed_behavior() {
    run(|driver| {
        async move {
            let environment = Environment::new(driver, None, ExecutorOptions::default());
            environment.install(1).await;
            let reference = environment.reference().await;
            let candidate = environment
                .prepare(environment.source(2, 1, true, false))
                .await;
            let error = environment
                .host
                .reload(vec![candidate], ReloadOptions::default())
                .await
                .unwrap_err();
            assert!(error.message.contains("candidate refused"));
            assert_eq!(
                reference.call_json("read", None, None).await.unwrap(),
                Some(json!([1, 1, 41, 1, true]))
            );
            assert_eq!(*environment.events.borrow(), vec![json!(["open", 1])]);
            reference.release();
            environment.close().await;
            assert_eq!(
                *environment.events.borrow(),
                vec![json!(["open", 1]), json!(["close", 1, 41])]
            );
            environment.assert_closed();
        }
        .boxed_local()
    });
}

#[test]
fn versioned_restart_closes_replaced_value_with_original_closer() {
    run(|driver| {
        async move {
            let environment = Environment::new(driver, None, ExecutorOptions::default());
            environment.install(1).await;
            let reference = environment.reference().await;
            let candidate = environment
                .prepare(environment.source(2, 2, false, false))
                .await;
            environment.reload(candidate).await;
            assert_eq!(
                reference.call_json("read", None, None).await.unwrap(),
                Some(json!([2, 2, 42, 2, true]))
            );
            reference.release();
            environment.close().await;
            assert!(
                environment.host.errors().is_empty(),
                "retirement errors: {:?}",
                environment.host.errors()
            );
            let events = environment.events.borrow();
            assert_eq!(events.iter().filter(|event| event[0] == "open").count(), 2);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == json!(["close", 1, 41]))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == json!(["close", 2, 42]))
                    .count(),
                1
            );
            drop(events);
            environment.assert_closed();
        }
        .boxed_local()
    });
}

#[test]
fn pinned_engine_budget_refuses_rotation_without_closing_the_kept_resource() {
    run(|driver| {
        async move {
            let environment = Environment::new(
                driver.clone(),
                None,
                ExecutorOptions {
                    max_evaluations: 2,
                    ..Default::default()
                },
            );
            environment.install(1).await;
            let reference = environment.reference().await;
            let candidate = environment
                .prepare(environment.source(2, 1, false, false))
                .await;
            environment.reload(candidate).await;
            let error = environment
                .executor
                .prepare(
                    environment.host.id(),
                    BundledJsArtifact::new(
                        "over-budget",
                        environment.source(3, 1, false, false),
                        vec!["default".into()],
                    )
                    .unwrap(),
                    "default".into(),
                )
                .await
                .err()
                .unwrap();
            assert_eq!(error.reason, "rotation_required");
            let error = Executor::new(driver, ExecutorOptions::default())
                .err()
                .unwrap();
            assert_eq!(error.reason, "engine_thread");
            assert_eq!(
                reference.call_json("read", None, None).await.unwrap(),
                Some(json!([2, 1, 41, 2, true]))
            );
            assert_eq!(*environment.events.borrow(), vec![json!(["open", 1])]);
            reference.release();
            environment.close().await;
            environment.assert_closed();
        }
        .boxed_local()
    });
}

#[test]
fn detached_task_root_keeps_resource_open_after_task_callback_returns_and_close_waiter_drops() {
    run(|driver| {
        async move {
            let (release, wait) = oneshot::channel();
            let environment = Environment::new(driver, Some(wait), ExecutorOptions::default());
            let source = format!(
                r#"
const token={token}, monitor={monitor};
export default {{name:'resource',uses:[monitor],setup(scope,declare){{
    declare.serve(token,(input,self)=>{{
        const log=self.get(monitor,{{}});
        const resource=self.resource('held',{{
            deps:[log],open:()=>({{read:()=>41}}),
            async close(value){{
                if(globalThis.finishedRoot!==41)throw Error('resource closed before detached root');
                await scope.root(ctx=>log.record(['close',1,value.read()],ctx));
            }}
        }});
        self.task(async task=>{{
            const started=new Promise(resolve=>globalThis.rootStarted=resolve);
            globalThis.savedTask=task;
            void task.root(async ctx=>{{
                await log.record(['root-started'],ctx);
                globalThis.rootStarted();
                await log.wait(ctx);
                globalThis.finishedRoot=resource.value.read();
                try{{
                    await globalThis.savedTask.root(ctx=>log.record(['revived-task'],ctx));
                    throw Error('ended task admitted root');
                }}catch(error){{
                    if(error.reason!=='activity_stopped'&&error.reason!=='expired')throw error;
                }}
            }});
            await started;
        }});
        return {{methods:{{read:()=>[resource.value.read()]}},state:{{}}}};
    }},{{lifetime:'host'}});
}}}};
"#,
                token = serde_json::to_string(environment.service.descriptor()).unwrap(),
                monitor = serde_json::to_string(environment.monitor.descriptor()).unwrap()
            );
            let facet = environment.prepare(source).await;
            environment
                .host
                .install_executable(facet, LoadOptions::default())
                .unwrap()
                .await
                .unwrap();
            let reference = environment.reference().await;
            while !environment
                .events
                .borrow()
                .iter()
                .any(|event| *event == json!(["root-started"]))
            {
                tokio::task::yield_now().await;
            }
            tokio::task::yield_now().await;
            reference.release();
            let close = environment.host.close().boxed_local();
            let Either::Right(((), close)) = select(
                close,
                tokio::time::sleep(Duration::from_millis(10)).boxed_local(),
            )
            .await
            else {
                panic!("close discarded the detached task-root entry");
            };
            assert!(
                environment
                    .events
                    .borrow()
                    .iter()
                    .all(|event| event[0] != "close")
            );
            drop(close);
            release.send(()).unwrap();
            environment.close().await;
            assert!(
                environment
                    .events
                    .borrow()
                    .iter()
                    .all(|event| event[0] != "revived-task")
            );
            assert_eq!(
                environment
                    .events
                    .borrow()
                    .iter()
                    .filter(|event| **event == json!(["close", 1, 41]))
                    .count(),
                1
            );
            environment.assert_closed();
        }
        .boxed_local()
    });
}

#[test]
fn repeated_same_executable_definition_has_bounded_setup_owners_and_kept_cleanup_roots() {
    run(|driver| {
        async move {
            let environment = Environment::new(
                driver,
                None,
                ExecutorOptions {
                    max_evaluations: 1,
                    max_capabilities: 64,
                    ..Default::default()
                },
            );
            let facet = environment
                .prepare(environment.source(1, 1, false, false))
                .await;
            environment
                .host
                .install_executable(facet.clone(), LoadOptions::default())
                .unwrap()
                .await
                .unwrap();
            let reference = environment.reference().await;
            let mut previous = None;
            for _ in 0..40 {
                environment.reload(facet.clone()).await;
                assert_eq!(
                    reference
                        .call_json("read", None, None)
                        .await
                        .unwrap()
                        .unwrap()[1],
                    json!(1)
                );
                tokio::task::yield_now().await;
                let stats = environment.executor.stats();
                if let Some((caps, callbacks)) = previous {
                    assert!(
                        stats.capabilities <= caps + 4,
                        "unbounded setup caps: {stats:?}"
                    );
                    assert!(
                        stats.callbacks <= callbacks + 4,
                        "unbounded setup callbacks: {stats:?}"
                    );
                }
                previous.get_or_insert((stats.capabilities, stats.callbacks));
            }
            assert_eq!(environment.executor.stats().evaluations, 1);
            drop(facet);
            assert_eq!(*environment.events.borrow(), vec![json!(["open", 1])]);
            reference.release();
            environment.close().await;
            assert_eq!(
                *environment.events.borrow(),
                vec![json!(["open", 1]), json!(["close", 1, 41])]
            );
            environment.assert_closed();
        }
        .boxed_local()
    });
}

#[test]
fn retired_old_javascript_call_keeps_its_resource_until_physical_settlement() {
    run(|driver| {
        async move {
            let (release, wait) = oneshot::channel();
            let environment = Environment::new(driver, Some(wait), ExecutorOptions::default());
            let source = environment.source(1, 1, false, false).replace(
                "read(ctx) {",
                r#"async read(ctx) {
                await log.record(['call-started'], ctx);
                await log.wait(ctx);
                try {
                    await log.record(['late-call-effect'], ctx);
                    throw Error('retired Context admitted an effect');
                } catch (error) {
                    if (error.code !== 'superseded' && error.code !== 'aborted') throw error;
                }
"#,
            );
            let first = environment.prepare(source).await;
            environment
                .host
                .install_executable(first, LoadOptions::default())
                .unwrap()
                .await
                .unwrap();
            let reference = environment.reference().await;
            let pending = reference.call_json("read", None, None).boxed_local();
            let Either::Right(((), pending)) = select(
                pending,
                tokio::time::sleep(Duration::from_millis(10)).boxed_local(),
            )
            .await
            else {
                panic!("old callback did not wait for its native gate");
            };
            assert!(
                environment
                    .events
                    .borrow()
                    .contains(&json!(["call-started"]))
            );
            let next = environment
                .prepare(environment.source(2, 2, false, false))
                .await;
            let report = environment
                .host
                .reload(
                    vec![next],
                    ReloadOptions {
                        gate_deadline: Duration::from_millis(5),
                        retire_deadline: Duration::from_millis(5),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(report.current_version(), 2);
            // A compatible V2 commit keeps V1 callable. V3 must retire that previous
            // epoch first, without mistaking cancellation for physical settlement.
            let third = environment
                .prepare(environment.source(3, 2, false, false))
                .await;
            let report = environment
                .host
                .reload(
                    vec![third],
                    ReloadOptions {
                        retire_deadline: Duration::from_millis(5),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(report.current_version(), 3);
            assert!(
                !environment
                    .events
                    .borrow()
                    .contains(&json!(["close", 1, 41]))
            );
            release.send(()).unwrap();
            assert_eq!(pending.await.unwrap(), Some(json!([1, 1, 41, 1, false])));
            assert!(
                !environment
                    .events
                    .borrow()
                    .contains(&json!(["late-call-effect"]))
            );
            assert_eq!(
                reference.call_json("read", None, None).await.unwrap(),
                Some(json!([3, 2, 42, 3, true]))
            );
            reference.release();
            environment.close().await;
            assert_eq!(
                environment
                    .events
                    .borrow()
                    .iter()
                    .filter(|event| **event == json!(["close", 1, 41]))
                    .count(),
                1
            );
            environment.assert_closed();
        }
        .boxed_local()
    });
}

#[test]
fn exclusive_reload_gate_timeout_keeps_old_javascript_resource_and_method_installed() {
    run(|driver| {
        async move {
            let (release, wait) = oneshot::channel();
            let environment = Environment::new(driver, Some(wait), ExecutorOptions::default());
            let source = environment
                .source(1, 1, false, false)
                .replace("version: 1, deps:", "version: 1, exclusive: true, deps:")
                .replace("read(ctx) {", "async read(ctx) { await log.wait(ctx);");
            let first = environment.prepare(source).await;
            environment
                .host
                .install_executable(first, LoadOptions::default())
                .unwrap()
                .await
                .unwrap();
            let reference = environment.reference().await;
            let pending = reference.call_json("read", None, None).boxed_local();
            let Either::Right(((), pending)) = select(
                pending,
                tokio::time::sleep(Duration::from_millis(10)).boxed_local(),
            )
            .await
            else {
                panic!("old callback did not enter its native gate");
            };
            let candidate = environment
                .prepare(
                    environment
                        .source(2, 2, false, false)
                        .replace("version: 2, deps:", "version: 2, exclusive: true, deps:"),
                )
                .await;
            let error = environment
                .host
                .reload(
                    vec![candidate],
                    ReloadOptions {
                        gate_deadline: Duration::from_millis(5),
                        ..Default::default()
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(error.reason, "reload_gate");
            assert_eq!(*environment.events.borrow(), vec![json!(["open", 1])]);
            release.send(()).unwrap();
            assert_eq!(pending.await.unwrap(), Some(json!([1, 1, 41, 1, true])));
            reference.release();
            environment.close().await;
            assert_eq!(
                *environment.events.borrow(),
                vec![json!(["open", 1]), json!(["close", 1, 41])]
            );
            environment.assert_closed();
        }
        .boxed_local()
    });
}
