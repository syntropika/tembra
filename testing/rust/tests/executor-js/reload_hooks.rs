//! A real V8 middleware pipeline keeps its version snapshot across compatible reload.
use futures::{FutureExt, channel::oneshot, future::LocalBoxFuture};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_hooks::*;
use tembra_native::TokioDriver;

#[derive(Debug)]
struct Marker;
struct MarkerCodec;
impl ScopedCodec<Rc<Marker>> for MarkerCodec {
    fn encode(&self, value: Rc<Marker>) -> Result<NativeValue> {
        Ok(NativeValue::capability(value))
    }

    fn decode(&self, value: NativeValue) -> Result<Rc<Marker>> {
        value.downcast()
    }
}

#[derive(Clone, Debug)]
struct Trace {
    event: Value,
    root: u64,
    epoch: u64,
    principal: Value,
}

fn token(name: &str, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

fn source(
    contribution: &Contract,
    monitor: &Contract,
    hook: &HookDescriptor,
    version: u64,
) -> String {
    format!(
        r#"
const token = {token}, monitor = {monitor}, registry = {registry}, data = {data};
export default {{name: 'reload.middleware', uses: [monitor], setup(scope, declare) {{
    declare.serve(token, (input, self) => {{
        const log = self.get(monitor, {{}});
        const count = self.cell('count', {{schema: {{type: 'integer'}}, initial: 0}});
        const resource = self.resource('kept', {{
            version: 1, deps: [log],
            async open() {{
                await scope.root(ctx => log.record(['open', {version}], ctx));
                const value = {{origin: {version}}};
                globalThis.firstHookResource = value;
                return value;
            }},
            close: value => scope.root(ctx => log.record(['close', {version}, value.origin], ctx))
        }});
        self.registerMethod(registry, 'javascript', data, 'run');
        return {{methods: {{
            async run(ctx, args) {{
                count.change(ctx, draft => {{draft.value++;}});
                await log.record(['before', {version}, resource.value.origin, count.value], ctx);
                if ({version} === 1) await log.wait(ctx);
                const result = await args.next.call(args.input);
                await log.record(['after', {version}, resource.value.origin, count.value], ctx);
                return result;
            }},
            read() {{
                return [{version}, resource.value.origin, count.value,
                    resource.value === globalThis.firstHookResource];
            }}
        }}, state: {{count}}}};
    }}, {{lifetime: 'host'}});
}}}};
"#,
        token = serde_json::to_string(contribution.descriptor()).unwrap(),
        monitor = serde_json::to_string(monitor.descriptor()).unwrap(),
        registry = json!({"name":hook.registry().name(),"conflict":"error"}),
        data = json!({"hook":hook.fingerprint(),"priority":0,"profile":1})
    )
}

async fn prepare(host: &Host, executor: &Executor, source: String) -> ExecutableFacet {
    executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("reload.middleware", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap()
}

fn pipeline(
    host: &Host,
    manager: HookManager,
    definition: RustHookDefinition<Rc<Marker>, Rc<Marker>>,
    terminal: HookTerminal<Rc<Marker>, Rc<Marker>>,
    marker: Rc<Marker>,
) -> LocalBoxFuture<'static, Result<Rc<Marker>>> {
    let host = host.clone();
    async move {
        host.root(move |ctx| {
            async move {
                manager
                    .run_dynamic_middleware(&definition, marker, &ctx, terminal)
                    .await
            }
            .boxed_local()
        })
        .await
    }
    .boxed_local()
}

#[test]
fn active_v1_javascript_pipeline_survives_rejected_candidate_then_valid_v2_commit() {
    run(false);
}

#[test]
fn forced_hook_retirement_refuses_next_and_close_waits_for_actual_javascript_settlement() {
    run(true);
}

fn run(forced_retirement: bool) {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(
                Duration::from_secs(20),
                scenario(driver.clone(), forced_retirement),
            )
            .await
            .unwrap();
        }));
}

async fn scenario(driver: Rc<TokioDriver>, forced_retirement: bool) {
    let host = Host::new(
        HostOptions::new("reload-hooks", json!("host")),
        driver.clone(),
    );
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let marker_type = HookType::<Rc<Marker>>::native("reload.marker@1", |_| Ok(())).unwrap();
    let hook = HookDescriptor::new(
        "reload.hook",
        1,
        Strategy::Middleware,
        marker_type.profile().clone(),
        marker_type.profile().clone(),
        Some(TerminalProfile {
            input: marker_type.profile().clone(),
            result: marker_type.profile().clone(),
        }),
    )
    .unwrap();
    executor
        .register_hook(&hook, Rc::new(MarkerCodec), Rc::new(MarkerCodec))
        .unwrap();
    let definition =
        RustHookDefinition::from_descriptor(hook.clone(), marker_type.clone(), marker_type.clone())
            .unwrap();
    let contribution = token(
        "reload.contribution",
        BTreeMap::from([
            ("run".into(), hook.contribution_member()),
            (
                "read".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Portable(json!({"type":"array","items":{}}))),
                },
            ),
            (
                "count".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
        ]),
    );
    let monitor = token(
        "reload.monitor",
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
    );
    let events = Rc::new(RefCell::new(Vec::<Trace>::new()));
    let (release_gate, gate) = oneshot::channel();
    let gate = Rc::new(RefCell::new(Some(gate)));
    let (entered, wait_entered) = oneshot::channel();
    let entered = Rc::new(RefCell::new(Some(entered)));
    let (recorded, waiting, notification) = (events.clone(), gate.clone(), entered.clone());
    host.serve(
        monitor.clone(),
        Rc::new(move |_, _, _| {
            let (recorded, waiting, notification) =
                (recorded.clone(), waiting.clone(), notification.clone());
            Ok(Implementation::new()
                .json_method("record", move |event, ctx| {
                    recorded.borrow_mut().push(Trace {
                        event: event.unwrap(),
                        root: ctx.root_id(),
                        epoch: ctx.code_version(),
                        principal: ctx.principal().clone(),
                    });
                    async { Ok(None) }.boxed_local()
                })
                .json_method("wait", move |_, _| {
                    let gate = waiting.borrow_mut().take().expect("only V1 waits");
                    notification.borrow_mut().take().unwrap().send(()).unwrap();
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
    let terminals = Registry::new("reload.terminals", ConflictPolicy::Error).unwrap();
    let terminal_contract = token(
        "reload.terminal",
        BTreeMap::from([(
            "run".into(),
            Member::Method {
                args: marker_type.profile().clone(),
                result: marker_type.profile().clone(),
            },
        )]),
    );
    let terminal_events = events.clone();
    let registry = terminals.clone();
    host.serve(
        terminal_contract,
        Rc::new(move |_, _, declaration| {
            declaration.register_method(&registry, "terminal", json!({}), "run")?;
            let terminal_events = terminal_events.clone();
            Ok(
                Implementation::new().method("run", move |marker: Rc<Marker>, ctx| {
                    terminal_events.borrow_mut().push(Trace {
                        event: json!(["terminal"]),
                        root: ctx.root_id(),
                        epoch: ctx.code_version(),
                        principal: ctx.principal().clone(),
                    });
                    async move { Ok(marker) }.boxed_local()
                }),
            )
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    let saved = Rc::new(RefCell::new(None));
    let captured = saved.clone();
    host.install(
        NativeFacet::new("scope", vec![], move |scope, _| {
            *captured.borrow_mut() = Some(scope.clone());
            Ok(())
        })
        .unwrap(),
        LoadOptions::default(),
    )
    .unwrap()
    .await
    .unwrap();
    let manager = HookManager::new(
        saved.borrow_mut().take().unwrap(),
        driver.clone(),
        HookManagerOptions::default(),
    )
    .unwrap();
    let first = prepare(&host, &executor, source(&contribution, &monitor, &hook, 1)).await;
    host.install_executable(first, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(contribution.clone(), json!({}), GetOptions::default())
        .await
        .unwrap();
    let readable = reference.readable("count").unwrap();
    let publications = Rc::new(RefCell::new(Vec::new()));
    let observed = publications.clone();
    let subscription = readable
        .subscribe_events(move |event| {
            observed.borrow_mut().push(event);
            Ok(())
        })
        .unwrap();
    let table = host.registry(&terminals).unwrap().table(None).unwrap();
    let terminal = HookTerminal::new(
        table.get("terminal").unwrap().operation().unwrap().clone(),
        marker_type.clone(),
        marker_type.clone(),
    )
    .unwrap();
    let marker = Rc::new(Marker);
    let old_pipeline = pipeline(
        &host,
        manager.clone(),
        definition.clone(),
        terminal.clone(),
        marker.clone(),
    );
    let (completed, old_result) = oneshot::channel();
    driver.spawn(
        async move {
            completed.send(old_pipeline.await).unwrap();
        }
        .boxed_local(),
    );
    wait_entered.await.unwrap();
    assert_eq!(readable.snapshot().unwrap().value(), &json!(1));
    assert_eq!(
        reference.call_json("read", None, None).await.unwrap(),
        Some(json!([1, 1, 1, true]))
    );
    let registry_before = host.registry(hook.registry()).unwrap().table(None).unwrap();
    let fingerprint_before = reference.shape().unwrap().contract;

    // Reject the candidate before the second successful epoch. A third reload can legitimately
    // retire an older root before checking its candidate and is not this rollback assertion.
    let mut incompatible_members = contribution.members("").unwrap().clone();
    let Member::Method { result, .. } = hook.contribution_member() else {
        unreachable!()
    };
    incompatible_members.insert(
        "run".into(),
        Member::Method {
            args: Some(ValueSchema::Native("incompatible.hook.args@1".into())),
            result,
        },
    );
    let incompatible = token("reload.contribution", incompatible_members);
    let rejected = prepare(&host, &executor, source(&incompatible, &monitor, &hook, 99)).await;
    let error = host
        .reload(vec![rejected], ReloadOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::ContractMismatch, "{error:?}");
    assert_eq!(readable.snapshot().unwrap().value(), &json!(1));
    assert_eq!(reference.shape().unwrap().contract, fingerprint_before);
    assert_eq!(
        host.registry(hook.registry())
            .unwrap()
            .table(None)
            .unwrap()
            .len(),
        registry_before.len()
    );
    assert_eq!(
        reference.call_json("read", None, None).await.unwrap(),
        Some(json!([1, 1, 1, true]))
    );
    assert!(
        !events
            .borrow()
            .iter()
            .any(|event| event.event[0] == "close")
    );

    let second = prepare(&host, &executor, source(&contribution, &monitor, &hook, 2)).await;
    let report = host
        .reload(vec![second], ReloadOptions::default())
        .await
        .unwrap();
    assert_eq!(report.current_version(), 2);
    assert_eq!(report.changed(), &["reload.middleware"]);
    assert!(report.gated_objects().is_empty());
    assert_eq!(readable.snapshot().unwrap().value(), &json!(1));
    assert_eq!(
        reference.call_json("read", None, None).await.unwrap(),
        Some(json!([2, 1, 1, true]))
    );
    let current_table = host.registry(&terminals).unwrap().table(None).unwrap();
    let current_terminal = HookTerminal::new(
        current_table
            .get("terminal")
            .unwrap()
            .operation()
            .unwrap()
            .clone(),
        marker_type.clone(),
        marker_type,
    )
    .unwrap();
    let new_result = pipeline(&host, manager, definition, current_terminal, marker.clone())
        .await
        .unwrap();
    assert!(Rc::ptr_eq(&marker, &new_result));
    assert_eq!(readable.snapshot().unwrap().value(), &json!(2));
    if forced_retirement {
        let third = prepare(&host, &executor, source(&contribution, &monitor, &hook, 3)).await;
        let report = host
            .reload(
                vec![third],
                ReloadOptions {
                    retire_deadline: Duration::from_millis(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(report.current_version(), 3);
        assert!(report.lingering_physical() > 0);
        assert!(executor.stats().physical_jobs > 0);
        assert_eq!(
            reference.call_json("read", None, None).await.unwrap(),
            Some(json!([3, 1, 2, true]))
        );
        let mut closing = host.close().boxed_local();
        assert!(futures::poll!(closing.as_mut()).is_pending());
        drop(closing);
        tokio::task::yield_now().await;
        assert!(executor.stats().physical_jobs > 0);
        assert!(
            !events
                .borrow()
                .iter()
                .any(|event| event.event[0] == "close")
        );
    }
    release_gate.send(()).unwrap();
    let old_result = old_result.await.unwrap();
    if forced_retirement {
        assert_eq!(old_result.unwrap_err().code, ErrorCode::Superseded);
        host.close().await.unwrap();
    } else {
        assert!(Rc::ptr_eq(&marker, &old_result.unwrap()));
        assert_eq!(
            reference.call_json("read", None, None).await.unwrap(),
            Some(json!([2, 1, 2, true]))
        );
    }
    // Rebinding publishes an authoritative Reset, not a reinitialized cell value.
    assert_eq!(
        publications
            .borrow()
            .iter()
            .map(|event| event.snapshot().value().clone())
            .collect::<Vec<_>>(),
        if forced_retirement {
            vec![json!(0), json!(1), json!(1), json!(2), json!(2)]
        } else {
            vec![json!(0), json!(1), json!(1), json!(2)]
        },
    );
    assert!(publications.borrow().iter().any(|event| {
        matches!(event, StateEvent::Reset(snapshot) if snapshot.value() == &json!(1))
    }));
    let trace = events.borrow().clone();
    let before = trace
        .iter()
        .filter(|event| event.event[0] == "before")
        .collect::<Vec<_>>();
    assert_eq!(before.len(), 2);
    assert_eq!(before[0].event, json!(["before", 1, 1, 1]));
    assert_eq!(before[1].event, json!(["before", 2, 1, 2]));
    assert_ne!(before[0].root, before[1].root);
    for (version, owner) in [(1, before[0]), (2, before[1])] {
        let continuation = trace
            .iter()
            .filter(|event| event.root == owner.root)
            .collect::<Vec<_>>();
        let refused_next = forced_retirement && version == 1;
        assert_eq!(continuation.len(), if refused_next { 1 } else { 3 });
        assert!(
            continuation
                .iter()
                .all(|event| event.epoch == version && event.principal == json!("host"))
        );
        assert_eq!(
            continuation
                .iter()
                .any(|event| event.event == json!(["terminal"])),
            !refused_next,
        );
        assert_eq!(
            continuation
                .iter()
                .any(|event| event.event[0] == "after" && event.event[1] == version),
            !refused_next,
        );
    }
    assert_eq!(
        trace
            .iter()
            .filter(|event| event.event[0] == "open")
            .count(),
        1
    );
    drop(trace);
    drop(subscription);
    reference.release();
    host.close().await.unwrap();
    let closes = events
        .borrow()
        .iter()
        .filter(|event| event.event[0] == "close")
        .map(|event| event.event.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        closes,
        vec![json!(["close", if forced_retirement { 3 } else { 2 }, 1])]
    );
    let stats = executor.stats();
    assert_eq!(
        (
            stats.callbacks,
            stats.capabilities,
            stats.contexts,
            stats.queued_jobs,
            stats.physical_jobs
        ),
        (0, 0, 0, 0, 0)
    );
}
