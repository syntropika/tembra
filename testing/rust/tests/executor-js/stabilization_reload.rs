//! Distinct evaluated ESM generations under calls, rejected preparation and physical retirement.
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

struct Gate {
    release: oneshot::Sender<()>,
    entered: oneshot::Receiver<()>,
}
struct Waiting {
    release: oneshot::Receiver<()>,
    entered: oneshot::Sender<()>,
}
type Events = Rc<RefCell<Vec<Value>>>;

fn contract(name: &str, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}
fn method(args: Option<ValueSchema>, result: Option<ValueSchema>) -> Member {
    Member::Method { args, result }
}
fn array() -> Option<ValueSchema> {
    Some(ValueSchema::Portable(json!({"type":"array","items":{}})))
}

struct Fixture {
    host: Host,
    executor: Executor,
    driver: Rc<TokioDriver>,
    contribution: Rc<Contract>,
    monitor: Rc<Contract>,
    hook: HookDescriptor,
    manager: HookManager,
    definition: RustHookDefinition<Rc<Marker>, Rc<Marker>>,
    terminals: Registry,
    marker_type: HookType<Rc<Marker>>,
    marker: Rc<Marker>,
    events: Events,
    waiting: Rc<RefCell<Option<Waiting>>>,
}
impl Fixture {
    async fn new(driver: Rc<TokioDriver>) -> Self {
        let host = Host::new(
            HostOptions::new("stabilization", json!(null)),
            driver.clone(),
        );
        let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
        let marker_type =
            HookType::native("stabilization.marker@1", |_: &Rc<Marker>| Ok(())).unwrap();
        let hook = HookDescriptor::new(
            "stabilization.hook",
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
        let definition = RustHookDefinition::from_descriptor(
            hook.clone(),
            marker_type.clone(),
            marker_type.clone(),
        )
        .unwrap();
        let contribution = contract(
            "stabilization.contribution",
            BTreeMap::from([
                ("run".into(), hook.contribution_member()),
                ("read".into(), method(None, array())),
                (
                    "count".into(),
                    Member::State {
                        schema: json!({"type":"integer"}),
                    },
                ),
            ]),
        );
        let monitor = contract(
            "stabilization.monitor",
            BTreeMap::from([
                ("record".into(), method(array(), None)),
                ("wait".into(), method(None, None)),
            ]),
        );
        let events = Rc::new(RefCell::new(Vec::new()));
        let waiting = Rc::new(RefCell::new(None::<Waiting>));
        let (recorded, pending) = (events.clone(), waiting.clone());
        host.serve(
            monitor.clone(),
            Rc::new(move |_, _, _| {
                let (recorded, pending) = (recorded.clone(), pending.clone());
                Ok(Implementation::new()
                    .json_method("record", move |event, _| {
                        recorded.borrow_mut().push(event.unwrap());
                        async { Ok(None) }.boxed_local()
                    })
                    .json_method("wait", move |_, _| {
                        let pending = pending.borrow_mut().take();
                        async move {
                            if let Some(pending) = pending {
                                pending.entered.send(()).unwrap();
                                // Deliberately ignore cancellation until the explicit physical gate.
                                pending.release.await.unwrap();
                            }
                            Ok(None)
                        }
                        .boxed_local()
                    }))
            }),
            ServeOptions::default(),
        )
        .unwrap();
        let terminals = Registry::new("stabilization.terminals", ConflictPolicy::Error).unwrap();
        let registry = terminals.clone();
        let terminal_contract = contract(
            "stabilization.terminal",
            BTreeMap::from([(
                "run".into(),
                method(marker_type.profile().clone(), marker_type.profile().clone()),
            )]),
        );
        let terminal_events = events.clone();
        host.serve(
            terminal_contract,
            Rc::new(move |_, _, declaration| {
                declaration.register_method(&registry, "terminal", json!({}), "run")?;
                let terminal_events = terminal_events.clone();
                Ok(
                    Implementation::new().method("run", move |value: Rc<Marker>, ctx| {
                        terminal_events
                            .borrow_mut()
                            .push(json!(["terminal", ctx.code_version()]));
                        async move { Ok(value) }.boxed_local()
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
        let capture = saved.clone();
        host.install(
            NativeFacet::new("stabilization.scope", vec![], move |scope, _| {
                *capture.borrow_mut() = Some(scope.clone());
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
        Self {
            host,
            executor,
            driver,
            contribution,
            monitor,
            hook,
            manager,
            definition,
            terminals,
            marker_type,
            marker: Rc::new(Marker),
            events,
            waiting,
        }
    }
    fn source(&self, generation: u64, fail: bool) -> String {
        format!(
            r#"
const token={token}, monitor={monitor}, registry={registry}, data={data};
export default {{name:'stabilization.js',uses:[monitor],setup(scope,declare){{
    declare.serve(token,(input,self)=>{{
        const log=self.get(monitor,{{}});
        const count=self.cell('count',{{schema:{{type:'integer'}},initial:0}});
        const kept=self.resource('kept',{{version:1,deps:[log],
            async open(){{
                await scope.root(ctx=>log.record(['open-kept',{generation}],ctx));
                const value={{origin:{generation},closed:false}};
                globalThis.stabilizationKept=value;
                return value;
            }},
            async close(value){{
                value.closed=true;
                await scope.root(ctx=>log.record(['close-kept',{generation},value.origin],ctx));
            }}
        }});
        const rolling=self.resource('rolling',{{version:{generation},deps:[log],
            async open(){{
                await scope.root(ctx=>log.record(['open-rolling',{generation}],ctx));
                return {{origin:{generation},closed:false}};
            }},
            async close(value){{
                value.closed=true;
                await scope.root(ctx=>log.record(['close-rolling',{generation},value.origin],ctx));
            }}
        }});
        if ({fail}) throw Error('deliberate candidate factory refusal');
        self.registerMethod(registry,'javascript',data,'run');
        return {{methods:{{
            async run(ctx,args){{
                const value=rolling.value;
                count.change(ctx,draft=>{{draft.value++;}});
                await log.record(['before',{generation},ctx.codeVersion],ctx);
                await log.wait(ctx);
                if(value.closed || kept.value.closed) throw Error('closed beneath physical work');
                const result=await args.next.call(args.input);
                await log.record(['after',{generation},ctx.codeVersion],ctx);
                return result;
            }},
            read(ctx){{return [{generation},count.value,kept.value.origin,
                kept.value===globalThis.stabilizationKept,rolling.value.origin,ctx.codeVersion];}}
        }},state:{{count}}}};
    }},{{lifetime:'host'}});
}}}};
"#,
            token = serde_json::to_string(self.contribution.descriptor()).unwrap(),
            monitor = serde_json::to_string(self.monitor.descriptor()).unwrap(),
            registry = json!({"name":self.hook.registry().name(),"conflict":"error"}),
            data = json!({"hook":self.hook.fingerprint(),"priority":0,"profile":1}),
        )
    }
    async fn prepare(&self, generation: u64, fail: bool) -> ExecutableFacet {
        self.executor
            .prepare(
                self.host.id(),
                BundledJsArtifact::new(
                    format!("stabilization-{generation}"),
                    self.source(generation, fail),
                    vec!["default".into()],
                )
                .unwrap(),
                "default".into(),
            )
            .await
            .unwrap()
    }
    fn pipeline(&self) -> LocalBoxFuture<'static, Result<Rc<Marker>>> {
        let table = self
            .host
            .registry(&self.terminals)
            .unwrap()
            .table(None)
            .unwrap();
        let terminal = HookTerminal::new(
            table.get("terminal").unwrap().operation().unwrap().clone(),
            self.marker_type.clone(),
            self.marker_type.clone(),
        )
        .unwrap();
        let (host, manager, definition, marker) = (
            self.host.clone(),
            self.manager.clone(),
            self.definition.clone(),
            self.marker.clone(),
        );
        async move {
            host.root(move |ctx| {
                async move {
                    manager
                        .run_middleware(&definition, marker, &ctx, terminal)
                        .await
                }
                .boxed_local()
            })
            .await
        }
        .boxed_local()
    }
    fn hold_pipeline(&self) -> (Gate, oneshot::Receiver<Result<Rc<Marker>>>) {
        assert!(self.waiting.borrow().is_none());
        let (release, released) = oneshot::channel();
        let (entered, entry) = oneshot::channel();
        *self.waiting.borrow_mut() = Some(Waiting {
            release: released,
            entered,
        });
        let pending = self.pipeline();
        let (send, receive) = oneshot::channel();
        self.driver.spawn(
            async move {
                let _ = send.send(pending.await);
            }
            .boxed_local(),
        );
        (
            Gate {
                release,
                entered: entry,
            },
            receive,
        )
    }
    async fn read(&self, reference: &Reference) -> Value {
        reference
            .call_json("read", None, None)
            .await
            .unwrap()
            .unwrap()
    }
    async fn reload(&self, generation: u64) -> ReloadReport {
        let candidate = self.prepare(generation, false).await;
        self.host
            .reload(
                vec![candidate],
                ReloadOptions {
                    retire_deadline: Duration::from_millis(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    }
    fn closed_rolling(&self, origin: u64) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|event| event[0] == "close-rolling" && event[2] == origin)
            .count()
    }
    async fn quiescent(&self) -> ExecutorStats {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let stats = self.executor.stats();
                if stats.contexts == 0 && stats.queued_jobs == 0 && stats.physical_jobs == 0 {
                    return stats;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
}

#[test]
fn evaluated_generations_preserve_owners_through_failure_and_forced_retirement() {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(30), scenario(driver.clone()))
                .await
                .unwrap();
        }));
}

async fn scenario(driver: Rc<TokioDriver>) {
    let fixture = Fixture::new(driver).await;
    let first = fixture.prepare(1, false).await;
    fixture
        .host
        .install_executable(first, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = fixture
        .host
        .get(
            fixture.contribution.clone(),
            json!({}),
            GetOptions::default(),
        )
        .await
        .unwrap();
    let readable = reference.readable("count").unwrap();
    let publications = Rc::new(RefCell::new(Vec::new()));
    let observed = publications.clone();
    let subscription = readable
        .subscribe_events(move |event| {
            observed.borrow_mut().push(event.snapshot().value().clone());
            Ok(())
        })
        .unwrap();
    let mut generation = 1;
    let mut epoch = 1;
    let mut count = 0;
    let mut plateau = None;
    let mut installed = vec![1];
    for cycle in 0..3 {
        let origin = generation;
        let (gate, result) = fixture.hold_pipeline();
        gate.entered.await.unwrap();
        count += 1;
        assert_eq!(readable.snapshot().unwrap().value(), &json!(count));
        let before = fixture.read(&reference).await;
        let revision = readable.snapshot().unwrap().revision();
        let table = fixture
            .host
            .registry(fixture.hook.registry())
            .unwrap()
            .table(None)
            .unwrap();
        let shape = reference.shape().unwrap().contract;
        generation += 1;
        let refused = fixture.prepare(generation, true).await;
        let error = fixture
            .host
            .reload(vec![refused], ReloadOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.reason, "javascript");
        assert_eq!(fixture.read(&reference).await, before);
        assert_eq!(readable.snapshot().unwrap().revision(), revision);
        assert_eq!(reference.shape().unwrap().contract, shape);
        assert_eq!(
            fixture
                .host
                .registry(fixture.hook.registry())
                .unwrap()
                .table(None)
                .unwrap()
                .len(),
            table.len()
        );
        assert_eq!(fixture.closed_rolling(origin), 0);
        generation += 1;
        epoch += 1;
        assert_eq!(fixture.reload(generation).await.current_version(), epoch);
        installed.push(generation);
        assert_eq!(
            fixture.read(&reference).await,
            json!([generation, count, 1, true, generation, epoch])
        );
        assert!(Rc::ptr_eq(
            &fixture.marker,
            &fixture.pipeline().await.unwrap()
        ));
        count += 1;
        generation += 1;
        epoch += 1;
        let report = fixture.reload(generation).await;
        installed.push(generation);
        assert_eq!(report.current_version(), epoch);
        assert!(report.lingering_physical() > 0);
        assert!(fixture.executor.stats().physical_jobs > 0);
        assert_eq!(fixture.closed_rolling(origin), 0);
        gate.release.send(()).unwrap();
        assert_eq!(
            result.await.unwrap().unwrap_err().code,
            ErrorCode::Superseded
        );
        fixture.quiescent().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.closed_rolling(origin) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.closed_rolling(origin), 1);
        // A final ordinary epoch drains any prior resource owner before plateau sampling.
        generation += 1;
        epoch += 1;
        assert_eq!(fixture.reload(generation).await.current_version(), epoch);
        installed.push(generation);
        assert!(Rc::ptr_eq(
            &fixture.marker,
            &fixture.pipeline().await.unwrap()
        ));
        count += 1;
        let stats = fixture.quiescent().await;
        if let Some((callbacks, capabilities)) = plateau {
            assert!(stats.callbacks <= callbacks, "cycle {cycle}: {stats:?}");
            assert!(
                stats.capabilities <= capabilities,
                "cycle {cycle}: {stats:?}"
            );
        } else {
            plateau = Some((stats.callbacks, stats.capabilities));
        }
        assert_eq!(stats.evaluations, generation as usize);
        assert_eq!(
            fixture.read(&reference).await,
            json!([generation, count, 1, true, generation, epoch])
        );
    }
    assert_eq!(generation, 13);
    assert_eq!(fixture.executor.stats().evaluations, 13);
    let admitted_bytes = (1..=13)
        .map(|generation| fixture.source(generation, generation % 4 == 2).len())
        .sum::<usize>();
    assert_eq!(fixture.executor.stats().source_bytes, admitted_bytes);
    let terminal_epochs = fixture
        .events
        .borrow()
        .iter()
        .filter(|event| event[0] == "terminal")
        .map(|event| event[1].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_epochs,
        vec![json!(2), json!(4), json!(5), json!(7), json!(8), json!(10)]
    );
    let opened = fixture
        .events
        .borrow()
        .iter()
        .filter(|event| event[0] == "open-rolling")
        .map(|event| event[1].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(opened, installed);
    assert_eq!(publications.borrow().last(), Some(&json!(count)));
    let (gate, pending) = fixture.hold_pipeline();
    gate.entered.await.unwrap();
    let mut closing = fixture.host.close().boxed_local();
    assert!(futures::poll!(closing.as_mut()).is_pending());
    tokio::task::yield_now().await;
    assert_eq!(fixture.closed_rolling(generation), 0);
    assert!(
        fixture
            .events
            .borrow()
            .iter()
            .all(|event| event[0] != "close-kept")
    );
    gate.release.send(()).unwrap();
    assert!(pending.await.unwrap().is_err());
    closing.await.unwrap();
    drop(subscription);
    reference.release();
    for origin in installed {
        assert_eq!(fixture.closed_rolling(origin), 1);
    }
    let kept_closes = fixture
        .events
        .borrow()
        .iter()
        .filter(|event| event[0] == "close-kept")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(kept_closes, vec![json!(["close-kept", 13, 1])]);
    let stats = fixture.executor.stats();
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
    assert!(
        fixture.host.errors().is_empty(),
        "{:?}",
        fixture.host.errors()
    );
    assert!(fixture.executor.take_errors().is_empty());
}
