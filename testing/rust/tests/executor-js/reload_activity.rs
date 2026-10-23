//! Actual managed listener/task callbacks, with core-owned lifetimes across reload.
use futures::{FutureExt, future::LocalBoxFuture};
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
async fn until(
    host: &Host,
    executor: &Executor,
    events: &Rc<RefCell<Vec<Value>>>,
    mut predicate: impl FnMut() -> bool,
) {
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "activity stalled; events={:?}; host={:?}; executor={:?}",
        events.borrow(),
        host.errors(),
        executor.take_errors()
    );
}
fn source(service: &Contract, monitor: &Contract, version: u64) -> String {
    format!(
        r#"
const service = {service}, monitor = {monitor};
function events() {{
    const listeners = new Set();
    return {{
        subscribes: 0, detaches: 0,
        subscribe(listener) {{
            this.subscribes++;
            listeners.add(listener);
            return () => {{this.detaches++; listeners.delete(listener);}};
        }},
        emit(value) {{for (const listener of listeners) listener(value);}}
    }};
}}
const direct = globalThis.directEvents ??= events();
const select = globalThis.selectEvents ??= value => value.events;
export default {{
    name: 'activities', uses: [monitor],
    setup(scope, declare) {{
        declare.serve(service, (input, self) => {{
            const log = self.get(monitor, {{}});
            const count = self.cell('count', {{schema: {{type: 'integer'}}, initial: 1}});
            const resource = self.resource('events', {{
                open: () => ({{events: events()}}),
                close: value => scope.root(ctx => log.record(['closed', {version}], ctx))
            }});
            self.on('count', count.readable, async (ctx, snapshot) => {{
                await log.record(['state', {version}, snapshot.value, ctx.codeVersion], ctx);
            }}, {{maxPending: 8}});
            self.onEvent('direct', direct, async (ctx, value) => {{
                await log.record(['direct', {version}, value, ctx.codeVersion], ctx);
            }});
            self.onResource('resource', resource, select, async (ctx, value) => {{
                await log.record(['resource', {version}, value, ctx.codeVersion], ctx);
            }});
            self.task(async task => {{
                await task.root(ctx => log.record(['task', {version}, ctx.codeVersion], ctx));
                await task.signal.cancelled;
                try {{
                    await task.root(ctx => log.record(['stale-task', {version}], ctx));
                    throw Error('stopped task opened a new root');
                }} catch (error) {{
                    if (error.reason !== 'activity_stopped') throw error;
                }}
            }});
            return {{methods: {{
                emit(ctx, value) {{
                    count.replace(ctx, value);
                    direct.emit(value);
                    resource.value.events.emit(value);
                }},
                stats() {{
                    const event = resource.value.events;
                    return [count.value, direct.subscribes, direct.detaches,
                        event.subscribes, event.detaches];
                }}
            }}, state: {{count}}}};
        }}, {{lifetime: 'host'}});
    }}
}};
"#,
        service = serde_json::to_string(service.descriptor()).unwrap(),
        monitor = serde_json::to_string(monitor.descriptor()).unwrap()
    )
}

#[test]
fn real_reload_reuses_event_sources_switches_handlers_and_stops_old_task_root_admission() {
    run(|driver| {
        async move {
            let host = Host::new(
                HostOptions::new("activities", json!("host")),
                driver.clone(),
            );
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let service = Rc::new(
                Contract::local(
                    "activities",
                    1,
                    json!({}),
                    BTreeMap::from([
                        (
                            "emit".into(),
                            Member::Method {
                                args: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                                result: None,
                            },
                        ),
                        (
                            "stats".into(),
                            Member::Method {
                                args: None,
                                result: Some(ValueSchema::Portable(
                                    json!({"type":"array","items":{}}),
                                )),
                            },
                        ),
                        (
                            "count".into(),
                            Member::State {
                                schema: json!({"type":"integer"}),
                            },
                        ),
                    ]),
                )
                .unwrap(),
            );
            let monitor = Rc::new(
                Contract::local(
                    "monitor",
                    1,
                    json!({}),
                    BTreeMap::from([(
                        "record".into(),
                        Member::Method {
                            args: Some(ValueSchema::Portable(json!({"type":"array","items":{}}))),
                            result: None,
                        },
                    )]),
                )
                .unwrap(),
            );
            let events = Rc::new(RefCell::new(Vec::<Value>::new()));
            let captured = events.clone();
            host.serve(
                monitor.clone(),
                Rc::new(move |_, _, _| {
                    let captured = captured.clone();
                    Ok(
                        Implementation::new().json_method("record", move |value, _| {
                            captured.borrow_mut().push(value.unwrap());
                            async { Ok(None) }.boxed_local()
                        }),
                    )
                }),
                ServeOptions::default(),
            )
            .unwrap();
            let prepare = |version| {
                executor.prepare_for_host(
                    &host,
                    BundledJsArtifact::new(
                        "activities",
                        source(&service, &monitor, version),
                        vec!["default".into()],
                    )
                    .unwrap(),
                    "default".into(),
                )
            };
            let first = prepare(1).await.unwrap();
            host.install_executable(first, LoadOptions::default())
                .unwrap()
                .await
                .unwrap();
            let reference = host
                .get(service.clone(), json!({}), GetOptions::default())
                .await
                .unwrap();
            until(&host, &executor, &events, || {
                events
                    .borrow()
                    .iter()
                    .any(|event| *event == json!(["task", 1, 1]))
            })
            .await;
            reference
                .call_json("emit", Some(json!(3)), None)
                .await
                .unwrap();
            until(&host, &executor, &events, || {
                events
                    .borrow()
                    .iter()
                    .any(|event| *event == json!(["resource", 1, 3, 1]))
            })
            .await;
            assert_eq!(
                reference.call_json("stats", None, None).await.unwrap(),
                Some(json!([3, 1, 0, 1, 0]))
            );
            let second = prepare(2).await.unwrap();
            host.reload(vec![second], ReloadOptions::default())
                .await
                .unwrap();
            until(&host, &executor, &events, || {
                events
                    .borrow()
                    .iter()
                    .any(|event| *event == json!(["task", 2, 2]))
            })
            .await;
            reference
                .call_json("emit", Some(json!(7)), None)
                .await
                .unwrap();
            until(&host, &executor, &events, || {
                events
                    .borrow()
                    .iter()
                    .any(|event| *event == json!(["resource", 2, 7, 2]))
            })
            .await;
            assert_eq!(
                reference.call_json("stats", None, None).await.unwrap(),
                Some(json!([7, 1, 0, 1, 0]))
            );
            for kind in ["state", "direct", "resource"] {
                assert!(
                    events
                        .borrow()
                        .iter()
                        .any(|event| *event == json!([kind, 2, 7, 2]))
                );
                assert!(
                    !events
                        .borrow()
                        .iter()
                        .any(|event| *event == json!([kind, 1, 7, 1]))
                );
            }
            reference.release();
            host.close().await.unwrap();
            assert!(events.borrow().iter().all(|event| event[0] != "stale-task"));
            assert_eq!(
                events
                    .borrow()
                    .iter()
                    .filter(|event| **event == json!(["closed", 2]))
                    .count(),
                1
            );
            let stats = executor.stats();
            assert_eq!(stats.callbacks, 0);
            assert_eq!(stats.physical_jobs, 0);
            assert_eq!(stats.contexts, 0);
            assert_eq!(stats.capabilities, 0);
            assert!(executor.take_errors().is_empty());
        }
        .boxed_local()
    });
}

#[test]
fn repeated_same_selector_and_restarted_resources_do_not_accumulate_source_owners() {
    run(|driver| {
        async move {
            let host = Host::new(
                HostOptions::new("restarted-events", json!(null)),
                driver.clone(),
            );
            let executor = Executor::new(
                driver,
                ExecutorOptions {
                    max_evaluations: 1,
                    max_callbacks: 40,
                    max_capabilities: 64,
                    ..Default::default()
                },
            )
            .unwrap();
            let token = Rc::new(
                Contract::local(
                    "restarted-events",
                    1,
                    json!({}),
                    BTreeMap::from([
                        (
                            "emit".into(),
                            Member::Method {
                                args: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                                result: None,
                            },
                        ),
                        (
                            "read".into(),
                            Member::Method {
                                args: None,
                                result: Some(ValueSchema::Portable(
                                    json!({"type":"array","items":{}}),
                                )),
                            },
                        ),
                    ]),
                )
                .unwrap(),
            );
            let source = format!(
                r#"
const token = {};
let pass = 0, opens = 0, closes = 0, active = 0, received = 0;
const select = resource => resource.events;
export default {{name: 'restarted-events', uses: [], setup(scope, declare) {{
    const version = ++pass;
    declare.serve(token, (input, self) => {{
        const resource = self.resource('events', {{version,
            open() {{
                opens++;
                const listeners = new Set();
                return {{events: {{
                    subscribe(listener) {{
                        active++;
                        listeners.add(listener);
                        return () => {{active--; listeners.delete(listener);}};
                    }},
                    emit(value) {{for (const listener of listeners) listener(value);}}
                }}}};
            }},
            close() {{closes++;}}
        }});
        self.onResource('events', resource, select, async (ctx, value) => {{received = value;}});
        return {{methods: {{
            emit(ctx, value) {{resource.value.events.emit(value);}},
            read() {{return [pass, opens, closes, active, received];}}
        }}, state: {{}}}};
    }}, {{lifetime: 'host'}});
}}}};
"#,
                serde_json::to_string(token.descriptor()).unwrap()
            );
            let facet = executor
                .prepare_for_host(
                    &host,
                    BundledJsArtifact::new("restarted-events", source, vec!["default".into()])
                        .unwrap(),
                    "default".into(),
                )
                .await
                .unwrap();
            host.install_executable(facet.clone(), LoadOptions::default())
                .unwrap()
                .await
                .unwrap();
            let reference = host
                .get(token, json!({}), GetOptions::default())
                .await
                .unwrap();
            let mut baseline = None;
            for step in 1..=40 {
                host.reload(vec![facet.clone()], ReloadOptions::default())
                    .await
                    .unwrap_or_else(|error| panic!("restart step {step}: {error:?}"));
                reference
                    .call_json("emit", Some(json!(step)), None)
                    .await
                    .unwrap();
                let values = tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        let values = reference
                            .call_json("read", None, None)
                            .await
                            .unwrap()
                            .unwrap();
                        if values[4] == json!(step) {
                            break values;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(values[0], json!(step + 1));
                assert_eq!(values[1], json!(step + 1));
                assert_eq!(values[3], json!(1));
                let stats = executor.stats();
                if step == 3 {
                    baseline = Some(stats);
                }
                if let Some(baseline) = baseline {
                    assert!(
                        stats.callbacks <= baseline.callbacks + 4,
                        "callback owners grew: {stats:?}"
                    );
                    assert!(
                        stats.capabilities <= baseline.capabilities + 4,
                        "source owners grew: {stats:?}"
                    );
                }
            }
            drop(facet);
            reference.release();
            host.close().await.unwrap();
            assert!(host.errors().is_empty(), "{:?}", host.errors());
            assert_eq!(executor.stats().callbacks, 0);
            assert_eq!(executor.stats().capabilities, 0);
            assert_eq!(executor.stats().physical_jobs, 0);
        }
        .boxed_local()
    });
}
