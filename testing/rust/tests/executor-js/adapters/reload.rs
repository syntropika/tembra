use super::*;
use futures::{
    channel::oneshot,
    future::{Either, select},
};
use std::cell::RefCell;

#[test]
fn actual_javascript_legacy_adapter_survives_reload_with_a_v1_promise_and_kept_resource() {
    run(|driver| versioned_reload(driver).boxed_local());
}

async fn versioned_reload(driver: Rc<TokioDriver>) {
    let host = Host::new(
        HostOptions::new("adapter-reload", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let method = || Member::Method {
        args: Some(portable(json!({"type":"integer"}))),
        result: Some(portable(json!({"type":"array","items":{"type":"integer"}}))),
    };
    let first = token(
        "adapter-reload",
        1,
        BTreeMap::from([("run".into(), method())]),
    );
    let second = token(
        "adapter-reload",
        2,
        BTreeMap::from([("runV2".into(), method())]),
    );
    let gate = token(
        "adapter-gate",
        1,
        BTreeMap::from([
            (
                "wait".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            ),
            (
                "closed".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            ),
        ]),
    );
    let (release, receive) = oneshot::channel();
    let wait = Rc::new(RefCell::new(Some(receive)));
    let entered = Rc::new(Cell::new(false));
    let closes = Rc::new(Cell::new(0));
    let observed = entered.clone();
    let closed = closes.clone();
    host.serve(
        gate.clone(),
        Rc::new(move |_, _, _| {
            let wait = wait.clone();
            let observed = observed.clone();
            let closed = closed.clone();
            Ok(Implementation::new()
                .json_method("wait", move |_, _| {
                    observed.set(true);
                    let wait = wait.borrow_mut().take().unwrap();
                    async move {
                        wait.await.unwrap();
                        Ok(None)
                    }
                    .boxed_local()
                })
                .json_method("closed", move |_, _| {
                    closed.set(closed.get() + 1);
                    async { Ok(None) }.boxed_local()
                }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source = |version: u64, bad: bool| {
        let provider = if version == 1 { &first } else { &second };
        let name = if version == 1 { "run" } else { "runV2" };
        let adapter = if version == 1 {
            String::new()
        } else {
            format!(
                ",accepts:[{}]",
                adapter(
                    provider,
                    &first,
                    if bad {
                        "Promise.resolve({methods:{run:current.methods.runV2}})"
                    } else {
                        "{methods:{run:current.methods.runV2}}"
                    }
                )
            )
        };
        format!(
            r#"
const provider={provider},gate={gate};
class PrivateValue{{#origin={version};origin(){{return this.#origin;}}}}
export default{{name:'adapters',uses:[gate],setup(scope,declare){{
    declare.serve(provider,(input,self)=>{{
        const dependency=self.get(gate,{{}});
        const count=self.cell('count',{{schema:{{type:'integer'}},initial:0}});
        const resource=self.resource('kept',{{deps:[dependency],open:()=>new PrivateValue(),
            close:()=>scope.root(ctx=>dependency.closed(ctx))}});
        return{{methods:{{async {name}(ctx,amount){{
            count.change(ctx,draft=>{{draft.value+=amount;}});
            if({version}===1)await dependency.wait(ctx);
            return[{version},ctx.codeVersion,count.value,resource.value.origin()];
        }}}}}};
    }},{{lifetime:'host'{adapter}}});
}}}};
"#,
            provider = descriptor(provider),
            gate = descriptor(&gate)
        )
    };
    host.install_executable(
        prepare(&host, &executor, source(1, false)).await,
        LoadOptions::default(),
    )
    .unwrap()
    .await
    .unwrap();
    let reference = host
        .get(first.clone(), json!({}), GetOptions::default())
        .await
        .unwrap();
    let pending = reference
        .call_json("run", Some(json!(1)), None)
        .boxed_local();
    let Either::Right(((), pending)) = select(
        pending,
        tokio::time::sleep(Duration::from_millis(5)).boxed_local(),
    )
    .await
    else {
        panic!("V1 promise did not wait for its actual native dependency");
    };
    assert!(entered.get());
    let refused = prepare(&host, &executor, source(2, true)).await;
    let error = host
        .reload(vec![refused], ReloadOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.reason, "async_declaration");
    assert_eq!(closes.get(), 0);
    let candidate = prepare(&host, &executor, source(2, false)).await;
    let report = host
        .reload(vec![candidate], ReloadOptions::default())
        .await
        .unwrap();
    assert_eq!(report.current_version(), 2);
    assert_eq!(
        reference
            .call_json("run", Some(json!(2)), None)
            .await
            .unwrap(),
        Some(json!([2, 2, 3, 1]))
    );
    assert_eq!(closes.get(), 0);
    release.send(()).unwrap();
    assert_eq!(pending.await.unwrap(), Some(json!([1, 1, 3, 1])));
    reference.release();
    host.close().await.unwrap();
    assert_eq!(closes.get(), 1);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_closed(&executor);
}
