use futures::{FutureExt, future::LocalBoxFuture};

use serde_json::json;

use std::{collections::BTreeMap, rc::Rc, time::Duration};

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
            tokio::time::timeout(Duration::from_secs(30), test(driver.clone()))
                .await
                .unwrap();
        }));
}

fn token(name: &str) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            1,
            json!({}),
            BTreeMap::from([(
                "run".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Portable(json!({
                    "type":"integer"}
                    ))),
                },
            )]),
        )
        .unwrap(),
    )
}

async fn install(host: &Host, executor: &Executor, source: String) {
    let facet = executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("review", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();

    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
}

#[test]
fn repeated_signal_reads_and_subscribe_unsubscribe_do_not_accumulate_live_handles() {
    run(|driver| {
        scenario_repeated_signal_reads_and_subscribe_unsubscribe_do_not_accumulate_live_handles(
            driver,
        )
        .boxed_local()
    });
}

#[test]
fn releasing_from_js_delivers_status_and_late_context_signal_remains_observable() {
    run(|driver| {
        scenario_releasing_from_js_delivers_status_and_late_context_signal_remains_observable(
            driver,
        )
        .boxed_local()
    });
}

#[test]
fn registry_publications_reuse_operation_slots_while_owners_remain_live() {
    run(|driver| {
        scenario_registry_publications_reuse_operation_slots_while_owners_remain_live(driver)
            .boxed_local()
    });
}

async fn scenario_repeated_signal_reads_and_subscribe_unsubscribe_do_not_accumulate_live_handles(
    driver: Rc<TokioDriver>,
) {
    let host = Host::new(HostOptions::new("review", json!("host")), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let contract = token("review");

    let source = format!(
        r#"
const token={};
export default {{
    name:'review',uses:[],setup(scope,d){{
        d.serve(token,(input,self)=>{{
            const count=self.cell('count',{{
                schema:{{
                    type:'integer'
                }}
                ,initial:0
            }}
            );
            return {{
                methods:{{
                    async run(ctx){{
                        for(let i=0;
                        i<1500;
                        i++){{
                            const stop=count.subscribe(()=>{{
                            }}
                            );
                            stop();
                            stop();
                            void ctx.signal.aborted;
                            void scope.signal.aborted;
                            void scope.signal.cancelled;
                        }}
                        return 7;
                    }}
                }}
                ,state:{{
                }}
            }}
            ;
        }}
        );
    }}
}}
;
"#,
        serde_json::to_string(contract.descriptor()).unwrap()
    );

    install(&host, &executor, source).await;
    let reference = host
        .get(contract, json!({}), GetOptions::default())
        .await
        .unwrap();

    for _ in 0..2 {
        assert_eq!(
            reference.call_json("run", None, None).await.unwrap(),
            Some(json!(7))
        );
        assert!(executor.stats().capabilities < 20);
        assert!(executor.stats().physical_jobs <= 1);
    }

    reference.release();
    host.close().await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().capabilities, 0);
}

async fn scenario_releasing_from_js_delivers_status_and_late_context_signal_remains_observable(
    driver: Rc<TokioDriver>,
) {
    let host = Host::new(HostOptions::new("review", json!("host")), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let contract = token("review");
    let dependency = token("dependency");

    host.serve(
        dependency.clone(),
        Rc::new(|_, _, _| {
            Ok(Implementation::new()
                .json_method("run", |_, _| async { Ok(Some(json!(1))) }.boxed_local()))
        }),
        ServeOptions::default(),
    )
    .unwrap();

    let source = format!(
        r#"
const token={},dependency={};
let old;
export default {{
    name:'review',uses:[],setup(scope,d){{
        d.serve(token,(input,self)=>{{
            const held=self.get(dependency,{{
            }}
            );
            let released=0;
            const stop=held.onStatus(status=>{{
                if(status==='released')released++;
            }}
            );
            return {{
                methods:{{
                    async run(ctx){{
                        if(old){{
                            if(old.signal!==old.signal||!old.signal.aborted){{
                                throw Error('late signal');
                            }}
                            return 2;
                        }}
                        old=ctx;
                        if(ctx.signal!==ctx.signal)throw Error('unstable signal');
                        held.release();
                        stop();
                        if(released!==1)throw Error('missing released notification');
                        return 1;
                    }}
                }}
                ,state:{{
                }}
            }}
            ;
        }}
        );
    }}
}}
;
"#,
        serde_json::to_string(contract.descriptor()).unwrap(),
        serde_json::to_string(dependency.descriptor()).unwrap()
    );

    install(&host, &executor, source).await;
    let reference = host
        .get(contract, json!({}), GetOptions::default())
        .await
        .unwrap();
    assert_eq!(
        reference.call_json("run", None, None).await.unwrap(),
        Some(json!(1))
    );
    assert_eq!(
        reference.call_json("run", None, None).await.unwrap(),
        Some(json!(2))
    );
    assert!(executor.take_errors().is_empty());
    reference.release();
    host.close().await.unwrap();
}

async fn scenario_registry_publications_reuse_operation_slots_while_owners_remain_live(
    driver: Rc<TokioDriver>,
) {
    let host = Host::new(
        HostOptions::new("registry-review", json!("host")),
        driver.clone(),
    );

    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();

    let registry = Registry::new("operations", ConflictPolicy::Error).unwrap();

    let published = token("published");

    let registered = registry.clone();

    host.serve(
        published,
        Rc::new(move |_, _, declare| {
            declare.register_method(&registered, "managed", json!({}), "run")?;

            Ok(Implementation::new()
                .json_method("run", |_, _| async { Ok(Some(json!(1))) }.boxed_local()))
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();

    let consumer = token("observer");

    let source = format!(
        r#"
const token={};
export default {{
    name:'observer',uses:[],setup(scope,d){{
        const view=scope.registry({{
            name:'operations',conflict:'error'
        }}
        );
        let latest,seen=0;
        view.subscribe(table=>{{
            latest=table;
            seen++;
        }}
        );
        d.serve(token,()=>({{
            methods:{{
                run:()=>seen
            }}
            ,state:{{
            }}
        }}
        ));
    }}
}}
;
"#,
        serde_json::to_string(consumer.descriptor()).unwrap()
    );

    install(&host, &executor, source).await;

    let reference = host
        .get(consumer, json!({}), GetOptions::default())
        .await
        .unwrap();

    for id in 0..1100 {
        let published = Rc::new(
            Contract::local(format!("published-{id}"), 1, json!({}), BTreeMap::new()).unwrap(),
        );

        let registry = registry.clone();

        host.serve(
            published.clone(),
            Rc::new(move |_, _, declare| {
                declare.register(&registry, format!("value-{id}"), json!({}))?;

                Ok(Implementation::new())
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
        .unwrap();

        let held = host
            .get(published, json!({}), GetOptions::default())
            .await
            .unwrap();

        held.release();

        assert!(executor.stats().capabilities < 20);
    }

    assert!(
        reference
            .call_json("run", None, None)
            .await
            .unwrap()
            .unwrap()
            .as_i64()
            .unwrap()
            > 1100
    );

    assert!(executor.take_errors().is_empty());

    reference.release();

    host.close().await.unwrap();

    assert_eq!(executor.stats().capabilities, 0);
}
