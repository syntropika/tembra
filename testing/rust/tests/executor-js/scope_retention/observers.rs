use super::*;

fn registry_contract(name: &str) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), BTreeMap::new()).unwrap())
}

async fn inspector(host: &Host, executor: &Executor) -> Reference {
    let token = Rc::new(
        Contract::local(
            "scope-inspector",
            1,
            json!({}),
            BTreeMap::from([(
                "read".into(),
                Member::Method {
                    args: Some(ValueSchema::Portable(json!({"type":"string"}))),
                    result: Some(ValueSchema::Portable(json!({}))),
                },
            )]),
        )
        .unwrap(),
    );
    let source = format!(
        r#"
const token={};
export default {{name:'scope-inspector',uses:[],setup(scope,declare){{
    declare.serve(token,()=>({{methods:{{read(ctx,operation){{
        if(operation==='signal')return globalThis.observerSignal.aborted;
        if(operation==='stop'){{globalThis.observerStop();globalThis.observerStop();return null;}}
        if(operation==='runs')return globalThis.observerRuns.map(run=>({{
            hits:run.hits.slice(),aborted:run.signal.aborted
        }}));
        if(operation==='reject'){{globalThis.refuseObserver=true;return null;}}
        if(operation==='allow'){{globalThis.refuseObserver=false;return null;}}
        if(operation==='reentrant'){{globalThis.stopObserverOnPublish=true;return null;}}
        return globalThis.observerNotifications.slice();
    }}}}}}));
}}}};
"#,
        serde_json::to_string(token.descriptor()).unwrap()
    );
    host.install_executable(
        prepare(host, executor, source).await,
        LoadOptions::default(),
    )
    .unwrap()
    .await
    .unwrap();
    host.get(token, json!({}), GetOptions::default())
        .await
        .unwrap()
}

async fn observer(host: &Host, executor: &Executor) -> ExecutableFacet {
    let source = r#"
const registry={name:'setup-observer',conflict:'error'};
export default {name:'setup-observer',uses:[],setup(scope){
    const signal=scope.signal,view=scope.registry(registry);
    const run={signal,hits:[]};
    (globalThis.observerRuns??=[]).push(run);
    globalThis.observerNotifications=run.hits;
    globalThis.observerSignal=signal;
    run.stop=view.subscribe(table=>{
        if(scope.signal!==signal||scope.registry(registry)!==view)
            throw Error('scope view changed during registry reentry');
        run.hits.push(table.entries.length);
        if(globalThis.stopObserverOnPublish&&table.entries.length>0)run.stop?.();
    });
    globalThis.observerStop=run.stop;
    if(globalThis.refuseObserver)throw Error('observer candidate refused');
}};
"#
    .to_owned();
    let definition = prepare(host, executor, source).await;
    host.install_executable(definition.clone(), LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    definition
}

async fn publish(host: &Host, name: &str) {
    let key = name.to_owned();
    let registry = Registry::new("setup-observer", ConflictPolicy::Error).unwrap();
    host.serve(
        registry_contract(name),
        Rc::new(move |_, _, declaration| {
            declaration.register(&registry, &key, json!(1))?;
            Ok(Implementation::new())
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    host.ready().await.unwrap();
}

#[test]
fn observer_only_registry_subscription_survives_setup_until_explicit_stop() {
    run(|driver| {
        async move {
            let host = Host::new(
                HostOptions::new("setup-observer", json!(null)),
                driver.clone(),
            );
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let definition = observer(&host, &executor).await;
            let inspection = inspector(&host, &executor).await;
            publish(&host, "entry-one").await;
            assert_eq!(
                inspection
                    .call_json("read", Some(json!("notifications")), None)
                    .await
                    .unwrap(),
                Some(json!([0, 1]))
            );
            assert_eq!(
                inspection
                    .call_json("read", Some(json!("stop")), None)
                    .await
                    .unwrap(),
                Some(json!(null))
            );
            publish(&host, "entry-two").await;
            assert_eq!(
                inspection
                    .call_json("read", Some(json!("notifications")), None)
                    .await
                    .unwrap(),
                Some(json!([0, 1]))
            );
            host.close().await.unwrap();
            drop(definition);
            tokio::task::yield_now().await;
            assert_closed(&executor);
        }
        .boxed_local()
    });
}

#[test]
fn observer_only_saved_signal_remains_live_during_the_installed_run() {
    run(|driver| {
        async move {
            let host = Host::new(
                HostOptions::new("setup-signal", json!(null)),
                driver.clone(),
            );
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let definition = observer(&host, &executor).await;
            let inspection = inspector(&host, &executor).await;
            assert_eq!(
                inspection
                    .call_json("read", Some(json!("signal")), None)
                    .await
                    .unwrap(),
                Some(json!(false))
            );
            host.close().await.unwrap();
            drop(definition);
            tokio::task::yield_now().await;
            assert_closed(&executor);
        }
        .boxed_local()
    });
}

#[test]
fn observer_registry_callback_can_stop_itself_reentrantly() {
    run(|driver| {
        async move {
            let host = Host::new(
                HostOptions::new("observer-reentry", json!(null)),
                driver.clone(),
            );
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let definition = observer(&host, &executor).await;
            let inspection = inspector(&host, &executor).await;
            inspection
                .call_json("read", Some(json!("reentrant")), None)
                .await
                .unwrap();
            let callbacks = executor.stats().callbacks;
            publish(&host, "entry-one").await;
            assert_eq!(executor.stats().callbacks, callbacks - 1);
            publish(&host, "entry-two").await;
            assert_eq!(
                inspection
                    .call_json("read", Some(json!("notifications")), None)
                    .await
                    .unwrap(),
                Some(json!([0, 1]))
            );
            inspection
                .call_json("read", Some(json!("stop")), None)
                .await
                .unwrap();
            host.close().await.unwrap();
            drop(definition);
            tokio::task::yield_now().await;
            assert_closed(&executor);
        }
        .boxed_local()
    });
}

#[test]
fn refused_observer_candidate_stops_only_itself_and_commit_stops_the_old_run() {
    run(|driver| {
        async move {
            let host = Host::new(
                HostOptions::new("observer-reload", json!(null)),
                driver.clone(),
            );
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let definition = observer(&host, &executor).await;
            let inspection = inspector(&host, &executor).await;
            publish(&host, "entry-one").await;
            inspection
                .call_json("read", Some(json!("reject")), None)
                .await
                .unwrap();
            let error = host
                .reload(vec![definition.clone()], ReloadOptions::default())
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::Invalid);
            publish(&host, "entry-two").await;
            assert_eq!(
                inspection
                    .call_json("read", Some(json!("runs")), None)
                    .await
                    .unwrap(),
                Some(json!([{ "hits":[0,1,2],"aborted":false},{"hits":[1],"aborted":true}]))
            );
            inspection
                .call_json("read", Some(json!("allow")), None)
                .await
                .unwrap();
            host.reload(vec![definition.clone()], ReloadOptions::default())
                .await
                .unwrap();
            publish(&host, "entry-three").await;
            assert_eq!(
                inspection
                    .call_json("read", Some(json!("runs")), None)
                    .await
                    .unwrap(),
                Some(
                    json!([{ "hits":[0,1,2],"aborted":true},{"hits":[1],"aborted":true},
                {"hits":[2,3],"aborted":false}])
                )
            );
            host.close().await.unwrap();
            assert!(host.errors().is_empty(), "{:?}", host.errors());
            drop(definition);
            tokio::task::yield_now().await;
            assert_closed(&executor);
        }
        .boxed_local()
    });
}

#[test]
fn dropping_observer_host_releases_native_slots_without_polling_driver() {
    run(|driver| {
        async move {
            let host = Host::new(
                HostOptions::new("observer-drop", json!(null)),
                driver.clone(),
            );
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let definition = observer(&host, &executor).await;
            assert_eq!(executor.stats().callbacks, 2); // Definition and live observer callback.
            assert!(executor.stats().capabilities > 0);
            drop(host);
            assert_eq!(executor.stats().callbacks, 1); // Only the explicitly retained definition.
            assert_eq!(executor.stats().capabilities, 0);
            drop(definition);
            assert_closed(&executor);
        }
        .boxed_local()
    });
}
