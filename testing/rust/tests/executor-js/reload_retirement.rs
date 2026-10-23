//! Four real epochs must not close a shared/kept JS resource beneath physical V1 work.
use futures::{FutureExt, channel::oneshot};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;

fn token(name: &str, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

fn source(service: &Contract, monitor: &Contract, epoch: u64, resource_version: u64) -> String {
    format!(
        r#"
const token={service}, monitor={monitor};
export default {{name:'retirement',uses:[monitor],setup(scope,declare){{
    declare.serve(token,(input,self)=>{{
        const log=self.get(monitor,{{}});
        const resource=self.resource('connection',{{
            version:{resource_version},deps:[log],
            async open(){{
                await scope.root(ctx=>log.record(['open',{epoch}],ctx));
                return {{origin:{epoch},closed:false}};
            }},
            async close(value){{
                value.closed=true;
                await scope.root(ctx=>log.record(['close',{epoch},value.origin],ctx));
            }}
        }});
        return {{methods:{{
            async pending(ctx){{
                const value=resource.value;
                await log.record(['entered',{epoch},ctx.codeVersion],ctx);
                await log.wait(ctx);
                return [value.origin,value.closed];
            }},
            peek:()=>[{epoch},resource.value.origin,resource.value.closed]
        }},state:{{}}}};
    }},{{lifetime:'host'}});
}}}};
"#,
        service = serde_json::to_string(service.descriptor()).unwrap(),
        monitor = serde_json::to_string(monitor.descriptor()).unwrap()
    )
}

async fn prepare(host: &Host, executor: &Executor, source: String) -> ExecutableFacet {
    executor
        .prepare_for_host(
            host,
            BundledJsArtifact::new("retirement", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap()
}

fn run(replay_v2: bool) {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(20), scenario(driver.clone(), replay_v2))
                .await
                .unwrap();
        }));
}

#[test]
fn shared_v1_v2_declarations_keep_replaced_resource_until_v1_promise_physically_settles() {
    run(false);
}

#[test]
fn kept_v1_resource_adopts_v2_closer_but_still_waits_for_v1_physical_work_at_v4() {
    run(true);
}

async fn scenario(driver: Rc<TokioDriver>, replay_v2: bool) {
    let host = Host::new(HostOptions::new("retirement", json!(null)), driver.clone());
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let method = || Member::Method {
        args: None,
        result: Some(ValueSchema::Portable(json!({"type":"array","items":{}}))),
    };
    let service = token(
        "retirement.service",
        BTreeMap::from([("pending".into(), method()), ("peek".into(), method())]),
    );
    let monitor = token(
        "retirement.monitor",
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
    let events = Rc::new(RefCell::new(Vec::<Value>::new()));
    let (release, gate) = oneshot::channel();
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
                .json_method("record", move |event, _| {
                    recorded.borrow_mut().push(event.unwrap());
                    async { Ok(None) }.boxed_local()
                })
                .json_method("wait", move |_, _| {
                    let gate = waiting.borrow_mut().take().unwrap();
                    notification.borrow_mut().take().unwrap().send(()).unwrap();
                    // Ignore cancellation; only the explicit gate settles this promise.
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
    let first = prepare(&host, &executor, source(&service, &monitor, 1, 1)).await;
    host.install_executable(first, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(service.clone(), json!({}), GetOptions::default())
        .await
        .unwrap();
    let active_reference = reference.clone();
    let (completed, completion) = oneshot::channel();
    driver.spawn(
        async move {
            let _ = completed.send(active_reference.call_json("pending", None, None).await);
        }
        .boxed_local(),
    );
    wait_entered.await.unwrap();
    let second = if replay_v2 {
        vec![prepare(&host, &executor, source(&service, &monitor, 2, 1)).await]
    } else {
        Vec::new()
    };
    assert_eq!(
        host.reload(second, ReloadOptions::default())
            .await
            .unwrap()
            .current_version(),
        2
    );
    for epoch in [3, 4] {
        let candidate = prepare(&host, &executor, source(&service, &monitor, epoch, 2)).await;
        let report = host
            .reload(
                vec![candidate],
                ReloadOptions {
                    retire_deadline: Duration::from_millis(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(report.current_version(), epoch);
        assert_eq!(
            reference.call_json("peek", None, None).await.unwrap(),
            Some(json!([epoch, 3, false]))
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(executor.stats().physical_jobs > 0);
        assert!(
            !events
                .borrow()
                .iter()
                .any(|event| event[0] == "close" && event[2] == 1),
            "closed beneath physical V1 promise at epoch{epoch}: {:?}",
            events.borrow()
        );
    }
    release.send(()).unwrap();
    assert_eq!(completion.await.unwrap().unwrap(), Some(json!([1, false])));
    let expected_closer = if replay_v2 { 2 } else { 1 };
    tokio::time::timeout(Duration::from_secs(2), async {
        while !events
            .borrow()
            .contains(&json!(["close", expected_closer, 1]))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        reference.call_json("peek", None, None).await.unwrap(),
        Some(json!([4, 3, false]))
    );
    reference.release();
    host.close().await.unwrap();
    assert_eq!(
        events
            .borrow()
            .iter()
            .filter(|event| event[0] == "close")
            .cloned()
            .collect::<Vec<_>>(),
        vec![json!(["close", expected_closer, 1]), json!(["close", 4, 3])]
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
