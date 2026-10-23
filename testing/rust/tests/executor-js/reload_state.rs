//! State migration enters real synchronous JS callbacks and private initializer snapshots.
use futures::{FutureExt, future::LocalBoxFuture};
use serde_json::json;
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
fn source(token: &Contract, next: bool, asynchronous: bool) -> String {
    let declaration = if next {
        format!(
            r#"
const count=self.cell('count',{{
    schema:{{type:'object',properties:{{value:{{type:'integer'}}}},required:['value'],
        additionalProperties:false}},
    version:3,
    initialize:()=>{{throw Error('kept initializer reran');}},
    migrate(value,previous){{
        if(previous!==1)throw Error('wrong previous version');
        const result={{value:value*2}};
        return {migration};
    }}
}});
if(count.value!==8)throw Error('factory did not read retained state');
const sibling=self.cell('sibling',{{schema:{{type:'integer'}},initialize:ctx=>{{
    globalThis.savedInitializerContext=ctx;
    return count.snapshot(ctx).value.value;
}}}});
"#,
            migration = if asynchronous {
                "Promise.resolve(result)"
            } else {
                "result"
            }
        )
    } else {
        "const count=self.cell('count',{schema:{type:'integer'},initial:5});".into()
    };
    let read = if next {
        "[count.value,sibling.value,ctx.codeVersion]"
    } else {
        "[count.value,ctx.codeVersion]"
    };
    format!(
        r#"
const token={token};
export default {{name:'state',uses:[],setup(scope,declare){{
    declare.serve(token,(input,self)=>{{
        {declaration}
        return {{methods:{{
            read(ctx){{return {read};}},
            set(ctx,value){{count.replace(ctx,value);}}
        }},state:{{count}}}};
    }},{{lifetime:'host'}});
}}}};
"#,
        token = serde_json::to_string(token.descriptor()).unwrap()
    )
}
async fn prepare(host: &Host, executor: &Executor, source: String) -> ExecutableFacet {
    executor
        .prepare_for_host(
            host,
            BundledJsArtifact::new("state", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap()
}
#[test]
fn migration_jump_and_candidate_snapshot_adopt_atomically_and_async_migration_refuses_unchanged() {
    run(|driver| {
        async move {
            let host = Host::new(HostOptions::new("migration", json!(null)), driver.clone());
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let token = Rc::new(
                Contract::local(
                    "state",
                    1,
                    json!({}),
                    BTreeMap::from([
                        (
                            "read".into(),
                            Member::Method {
                                args: None,
                                result: Some(ValueSchema::Portable(json!({}))),
                            },
                        ),
                        (
                            "set".into(),
                            Member::Method {
                                args: Some(ValueSchema::Portable(json!({}))),
                                result: None,
                            },
                        ),
                        ("count".into(), Member::State { schema: json!({}) }),
                    ]),
                )
                .unwrap(),
            );
            let first = prepare(&host, &executor, source(&token, false, false)).await;
            host.install_executable(first, LoadOptions::default())
                .unwrap()
                .await
                .unwrap();
            let reference = host
                .get(token.clone(), json!({}), GetOptions::default())
                .await
                .unwrap();
            reference
                .call_json("set", Some(json!(8)), None)
                .await
                .unwrap();
            let readable = reference.readable("count").unwrap();
            let retained = readable.snapshot().unwrap();
            let events = Rc::new(RefCell::new(Vec::new()));
            let captured = events.clone();
            let _subscription = readable
                .subscribe_events(move |event| {
                    captured.borrow_mut().push(event);
                    Ok(())
                })
                .unwrap();
            let before_events = events.borrow().len();
            let rejected = prepare(&host, &executor, source(&token, true, true)).await;
            let error = host
                .reload(vec![rejected], ReloadOptions::default())
                .await
                .unwrap_err();
            assert_eq!(error.reason, "async_declaration", "{error:?}");
            assert_eq!(readable.snapshot().unwrap().revision(), retained.revision());
            assert_eq!(readable.snapshot().unwrap().value(), &json!(8));
            assert_eq!(events.borrow().len(), before_events);
            let candidate = prepare(&host, &executor, source(&token, true, false)).await;
            host.reload(vec![candidate], ReloadOptions::default())
                .await
                .unwrap();
            assert_eq!(
                reference.call_json("read", None, None).await.unwrap(),
                Some(json!([{"value":16},16,2]))
            );
            assert_eq!(readable.snapshot().unwrap().value(), &json!({"value":16}));
            assert_eq!(retained.value(), &json!(8));
            assert!(
                events
                    .borrow()
                    .iter()
                    .any(|event| matches!(event, StateEvent::Reset(_)))
            );
            reference.release();
            host.close().await.unwrap();
            assert_eq!(executor.stats().callbacks, 0);
            assert_eq!(executor.stats().physical_jobs, 0);
            assert_eq!(executor.stats().contexts, 0);
        }
        .boxed_local()
    });
}
