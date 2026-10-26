#![cfg(feature = "hooks")]

use serde_json::{Value, json};
use std::{cell::RefCell, future::Future, rc::Rc, time::Duration};
use tembra::{
    Hook, Host, Orb, Service,
    core::{Code, ContextKey, OpaqueContextValue},
};

#[path = "context_values/storage.rs"]
mod storage;
mod support;
use support::Execution;

const TRACE: ContextKey<String> = ContextKey::new("lifecycle/trace", 1);
const SDK: &str = include_str!("../../../packages/ts/sdk/dist/embedded.js");

fn run_case(body: impl Future<Output = ()>) {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&tokio, async move {
        tokio::time::timeout(Duration::from_secs(5), body)
            .await
            .expect("hook observation settles within the test deadline");
    });
}

#[test]
fn physical_native_execution_awaits_javascript_terminal_observation_after_cancellation() {
    for cooperative in [false, true] {
        run_case(async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            let terminal = Hook::<Value, ()>::series("afterExecution");
            let declared = terminal.clone();
            let reports = Rc::new(RefCell::new(Vec::new()));
            let recorded = reports.clone();
            native
                .install(vec![
                    Orb::new("native", 1)
                        .hook(terminal)
                        .service(
                            Service::new("execution", move |self_| {
                                Ok((self_.hook(&declared)?, recorded.clone()))
                            })
                            .method(
                                "run",
                                |state, ctx, (outcome, fail): (String, bool)| async move {
                                    assert_eq!(ctx.principal()?, "alice");
                                    assert_eq!(ctx.value(&TRACE)?, Some("caller metadata".into()));
                                    if outcome == "Cancelled" {
                                        ctx.cancel()?;
                                        assert_eq!(ctx.check().unwrap_err().code, Code::Cancelled);
                                    }
                                    let payload = json!({"outcome":outcome,"observerError":fail});
                                    let observation = state.0.observe(&ctx, payload).await;
                                    if outcome == "Cancelled" {
                                        // The logical caller may already have ended its cancelled root.
                                        assert!(matches!(
                                            ctx.check().unwrap_err().code,
                                            Code::Cancelled | Code::Expired
                                        ));
                                    } else {
                                        ctx.check()?;
                                    }
                                    state.1.borrow_mut().push((outcome.clone(), observation));
                                    Ok(outcome)
                                },
                            ),
                        )
                        .service(Service::new("permitted", |_| Ok(())).method(
                            "ping",
                            |_, ctx, _: Vec<Value>| async move {
                                assert_eq!(ctx.principal()?, "alice");
                                assert_eq!(ctx.value(&TRACE)?, None);
                                Ok("permitted")
                            },
                        )),
                ])
                .unwrap();
            let mut runtime = execution.runtime(native.clone(), SDK).unwrap();
            runtime.evaluate(r#"(async()=>{
              globalThis.observed=[];
              await host.install({name:'js',apiVersion:1,
                requires:{native:{apiVersion:1,hooks:['afterExecution']}},
                contributes:{native:{afterExecution:async(ctx,payload)=>{
                  if(ctx.principal!=='alice'||ctx.value(Tembra.ContextKey.text('lifecycle/trace'))!==undefined)
                    throw Error('invalid terminal observation context');
                  await new Promise(resolve=>setTimeout(resolve,1));
                  if(await permitted.ping(ctx)!=='permitted')throw Error('native work failed');
                  if(await work.ping(ctx)!=='permitted')throw Error('javascript work failed');
                  observed.push(payload.outcome);
                  if(payload.observerError)throw Error('observer failed');
                }}},services:{work:{create:()=>({}),methods:{async ping(_,ctx){
                  await new Promise(resolve=>setTimeout(resolve,1));
                  return permitted.ping(ctx);
                }}}}
              });
              globalThis.execution=await host.get('execution','main',{principal:'alice'});
              globalThis.permitted=await host.get('permitted','main',{principal:'alice'});
              globalThis.work=await host.get('work','main',{principal:'alice'});
            })()"#).await.unwrap();
            for (outcome, fail) in [
                ("Completed", false),
                ("Failed", false),
                ("Cancelled", false),
                ("Cancelled", true),
            ] {
                let source = format!(
                    r#"(async()=>{{
                  const result=await host.root(ctx=>execution.run({outcome:?},{fail},
                    ctx.with(Tembra.ContextKey.text('lifecycle/trace'),'caller metadata')),
                    {{principal:'alice'}}).catch(error=>error.code);
                  while(host.diagnostics().calls>0)await new Promise(resolve=>setTimeout(resolve,0));
                  return result;
                }})()"#
                );
                let result = runtime.evaluate(source).await.unwrap();
                assert_eq!(
                    result,
                    if outcome == "Cancelled" {
                        "cancelled"
                    } else {
                        outcome
                    }
                );
                let reports = reports.borrow();
                let report = reports.last().unwrap();
                assert_eq!(report.0, outcome);
                assert_eq!(report.1.is_err(), fail);
                if fail {
                    assert!(
                        report
                            .1
                            .as_ref()
                            .unwrap_err()
                            .message
                            .contains("observer failed")
                    );
                }
                assert_eq!(native.kernel().diagnostics().roots, 0);
                assert_eq!(native.kernel().context_stats().0, 0);
            }
            assert_eq!(
                runtime.evaluate("observed").await.unwrap(),
                json!(["Completed", "Failed", "Cancelled", "Cancelled"])
            );
            runtime.close().await.unwrap();
            let diagnostics = native.kernel().diagnostics();
            assert_eq!(
                (
                    diagnostics.roots,
                    diagnostics.calls,
                    diagnostics.objects,
                    diagnostics.resources
                ),
                (0, 0, 0, 0)
            );
        });
    }
}

#[test]
fn terminal_observation_cannot_restore_a_closed_storage_token() {
    const SCOPE: ContextKey<OpaqueContextValue> = ContextKey::new("storage/pending", 1);
    for cooperative in [false, true] {
        run_case(async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            storage::install(&native);
            let reports = Rc::new(RefCell::new(Vec::new()));
            let recorded = reports.clone();
            let tokens = Rc::new(RefCell::new(None::<OpaqueContextValue>));
            let captured = tokens.clone();
            let terminal = Hook::<(), ()>::series("terminal");
            let declared = terminal.clone();
            native
                .install(vec![
                    Orb::new("terminal", 1)
                        .hook(terminal.clone())
                        .service(
                            Service::new("terminal", move |self_| {
                                Ok((self_.hook(&declared)?, captured.clone()))
                            })
                            .method(
                                "observe",
                                |state, ctx, _: Vec<Value>| async move {
                                    *state.1.borrow_mut() = ctx.value(&SCOPE)?;
                                    let original = ctx.get(
                                        "storage",
                                        vec!["main".into()],
                                        Default::default(),
                                    )?;
                                    ctx.cancel()?;
                                    assert_eq!(
                                        original
                                            .call::<_, i64>(&ctx, "append", (9_i64,))
                                            .await
                                            .unwrap_err()
                                            .code,
                                        Code::Cancelled
                                    );
                                    state.0.observe(&ctx, ()).await
                                },
                            ),
                        )
                        .contribute("terminal", &terminal, 0, move |ctx, ()| {
                            let tokens = tokens.clone();
                            let recorded = recorded.clone();
                            async move {
                                assert!(ctx.value(&SCOPE)?.is_none());
                                assert_eq!(ctx.principal()?, "alice");
                                let store =
                                    ctx.get("storage", vec!["main".into()], Default::default())?;
                                // Explicit transport cannot restore storage-owned transaction permission.
                                let token = tokens.borrow_mut().take().unwrap();
                                let rebound = ctx.with(&SCOPE, token)?;
                                let error = store
                                    .call::<_, i64>(&rebound, "append", (99_i64,))
                                    .await
                                    .unwrap_err();
                                recorded.borrow_mut().push(error.reason);
                                Ok(())
                            }
                        }),
                ])
                .unwrap();
            let mut runtime = execution.runtime(native.clone(), SDK).unwrap();
            let result = runtime
                .evaluate(
                    r#"(async()=>{
              const store=await host.get('storage','main',{principal:'alice'});
              const terminal=await host.get('terminal','main',{principal:'alice'});
              try {
                const outcome=await host.root(async ctx=>{
                  const Scope=Tembra.ContextKey.opaque('storage/pending');
                  const source=await store.open(ctx);
                  const scoped=ctx.with(Scope,Tembra.opaqueFrom(ctx,Scope,source));
                  await store.append(1,scoped);
                  await store.close(false,scoped);
                  await terminal.observe(scoped);
                },{principal:'alice'}).catch(error=>error.code);
                while(host.diagnostics().calls>0)await new Promise(resolve=>setTimeout(resolve,0));
                return {outcome,stats:await store.stats()};
              } finally {store.release();terminal.release();}
            })()"#,
                )
                .await
                .unwrap();
            assert_eq!(result["outcome"], "cancelled");
            assert_eq!(result["stats"]["rows"], json!([]));
            assert_eq!(result["stats"]["pendingScopes"], 0);
            assert_eq!(reports.borrow().as_slice(), ["storage_scope_closed"]);
            assert_eq!(native.kernel().context_stats().0, 0);
            runtime.close().await.unwrap();
            let diagnostics = native.kernel().diagnostics();
            assert_eq!(
                (diagnostics.roots, diagnostics.calls, diagnostics.resources),
                (0, 0, 0)
            );
        });
    }
}

#[test]
fn javascript_origin_observation_uses_shared_admission_and_survives_temporary_disposal() {
    use tembra::core::Requirement;
    for cooperative in [false, true] {
        run_case(async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            native
                .install(vec![Orb::new("temporary", 1).service(
                    Service::new("temporary", |_| Ok(())).method(
                        "read",
                        |_, ctx, _: Vec<Value>| async move {
                            assert_eq!(ctx.principal()?, "alice");
                            Ok(42_i32)
                        },
                    ),
                )])
                .unwrap();
            let mut runtime = execution.runtime(native.clone(), SDK).unwrap();
            runtime.evaluate(r#"(async()=>{
              globalThis.reports=[];globalThis.observed=[];globalThis.handles=new Map();
              const trace=Tembra.ContextKey.text('lifecycle/trace');
              const refused=async(operation,reason)=>{
                try{await operation();throw Error('admission unexpectedly succeeded');}
                catch(error){if(error.reason!==reason)throw error;}
              };
              await host.install({name:'jsowner',apiVersion:1,
                hooks:{terminal:{async run(ctx,input,participants){
                  for(const participant of participants)await participant(ctx,input);
                }}},
                services:{owner:{create(self){
                  const pair=[self.hook('terminal'),self.hook('terminal')];
                  handles.set(self.key[0],pair);return pair;
                },methods:{async run(pair,ctx,outcome,fail,publish){
                  globalThis.lastSource=ctx;
                  await refused(()=>handles.get('other')[0].observe(ctx,{}),'operation_owner');
                  const payload={outcome,observerError:fail,trace:ctx.value(trace),version:ctx.invocation.codeVersion};
                  if(publish)await host.install({name:'late',apiVersion:1,
                    requires:{jsowner:{apiVersion:1,hooks:['terminal']}},
                    contributes:{jsowner:{terminal(ctx,payload){if(payload.outcome==='Completed')throw Error('late plan participant ran');}}}});
                  if(outcome==='Cancelled')ctx.cancel();
                  let observerError;
                  const pending=pair[0].observe(ctx,payload).catch(error=>{observerError=String(error);});
                  await refused(()=>pair[1].observe(ctx,payload),'observation_admitted');
                  await pending;
                  await refused(()=>pair[1].observe(ctx,payload),'observation_admitted');
                  if(outcome==='Cancelled'){
                    let invalid=false;try{ctx.check();}catch{invalid=true;}
                    if(!invalid)throw Error('original context revived');
                  }
                  reports.push({outcome,observerError:observerError??null});
                  if(outcome==='Failed')throw Error('primary failure');
                  return outcome;
                },async descend(pair,ctx){
                  await refused(()=>pair[0].observe(ctx,{}),'observation_source');
                  return 'permitted';
                }}}}
              });
              await host.install({name:'jsfirst',apiVersion:1,
                requires:{jsowner:{apiVersion:1,hooks:['terminal']}},
                contributes:{jsowner:{terminal:{priority:-10,async handle(ctx,payload){
                  if(ctx.principal!=='alice'||ctx.value(trace)!==undefined)throw Error('invalid observer context');
                  if(ctx.invocation.codeVersion!==payload.version)throw Error('lost retained version');
                  await refused(()=>handles.get('main')[0].observe(ctx,payload),'observation_source');
                  const temporary=globalThis.initialTemporary??await ctx.get('temporary','transient');
                  globalThis.initialTemporary=undefined;
                  try{if(await temporary.read(ctx)!==42)throw Error('wrong native result');}
                  finally{temporary.release();}
                  await new Promise(resolve=>setTimeout(resolve,1));
                  ctx.check();
                }}}}
              });
              await host.install({name:'jsobserver',apiVersion:1,
                requires:{jsowner:{apiVersion:1,hooks:['terminal']}},
                contributes:{jsowner:{terminal:{priority:10,async handle(ctx,payload){
                  const descendant=globalThis.initialDescendant??await ctx.get('owner','descendant');
                  globalThis.initialDescendant=undefined;
                  try{if(await descendant.descend(ctx)!=='permitted')throw Error('descendant failed');}
                  finally{descendant.release();}
                  await new Promise(resolve=>setTimeout(resolve,1));
                  ctx.check();observed.push(payload.outcome);
                  if(payload.trace!=='source metadata')throw Error('safe payload metadata missing');
                  if(payload.observerError)throw Error('observer failure');
                }}}}
              });
              globalThis.owner=await host.get('owner','main',{principal:'alice'});
              globalThis.other=await host.get('owner','other',{principal:'alice'});
              // Retained roots may use already-live dependencies after a newer plan is published.
              globalThis.initialTemporary=await host.get('temporary','transient',{principal:'alice'});
              globalThis.initialDescendant=await host.get('owner','descendant',{principal:'alice'});
              await host.root(ctx=>refused(()=>handles.get('main')[0].observe(ctx,{}),'observation_source'),{principal:'alice'});
            })()"#).await.unwrap();
            let contribution = Hook::<Value, ()>::series("terminal");
            native
                .install(vec![
                    Orb::new("rustobserver", 1)
                        .requires(Requirement {
                            orb: "jsowner".into(),
                            api_version: 1,
                            methods: Default::default(),
                            hooks: ["terminal".into()].into(),
                        })
                        .contribute("jsowner", &contribution, 0, |ctx, payload| async move {
                            assert_eq!(ctx.principal()?, "alice");
                            assert_eq!(ctx.value(&TRACE)?, None);
                            assert_eq!(
                                ctx.invocation()?.code_version,
                                u32::try_from(payload["version"].as_u64().unwrap()).unwrap()
                            );
                            Ok(())
                        }),
                ])
                .unwrap();
            for (index, (outcome, fail)) in [
                ("Completed", false),
                ("Failed", true),
                ("Cancelled", false),
                ("Cancelled", true),
            ]
            .into_iter()
            .enumerate()
            {
                let script = format!(
                    r#"(async()=>{{
                  const result=await host.root(ctx=>owner.run({outcome:?},{fail},{},
                    ctx.with(Tembra.ContextKey.text('lifecycle/trace'),'source metadata')),
                    {{principal:'alice'}}).catch(error=>String(error));
                  while(host.diagnostics().calls>0)await new Promise(resolve=>setTimeout(resolve,0));
                  return {{result,report:reports.at(-1)}};
                }})()"#,
                    index == 0
                );
                let result = runtime.evaluate(script).await.unwrap();
                assert_eq!(result["report"]["outcome"], outcome);
                assert_eq!(
                    result["report"]["observerError"].is_string(),
                    fail,
                    "{result}"
                );
                if outcome == "Failed" {
                    assert!(
                        result["result"]
                            .as_str()
                            .unwrap()
                            .contains("primary failure")
                    );
                } else if outcome == "Cancelled" {
                    assert!(result["result"].as_str().unwrap().contains("cancelled"));
                } else {
                    assert_eq!(result["result"], outcome);
                }
                assert_eq!(native.kernel().diagnostics().roots, 0);
            }
            let result = runtime
                .evaluate(
                    r#"(async()=>{
              let expired;try{await handles.get('main')[0].observe(lastSource,{});}
              catch(error){expired=error.reason;}
              owner.release();other.release();return {expired,observed};
            })()"#,
                )
                .await
                .unwrap();
            assert_eq!(result["expired"], "call");
            assert_eq!(
                result["observed"],
                json!(["Completed", "Failed", "Cancelled", "Cancelled"])
            );
            runtime.close().await.unwrap();
            let diagnostics = native.kernel().diagnostics();
            assert_eq!(
                (
                    diagnostics.roots,
                    diagnostics.calls,
                    diagnostics.objects,
                    diagnostics.references,
                    diagnostics.resources
                ),
                (0, 0, 0, 0, 0)
            );
            assert_eq!(native.kernel().context_stats().0, 0);
        });
    }
}
