use futures::{FutureExt, channel::oneshot, future::LocalBoxFuture};

use serde_json::json;

use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Duration};

use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

use tembra_executor_js::*;

use tembra_native::TokioDriver;

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

fn run(test: impl FnOnce(Rc<TokioDriver>) -> LocalBoxFuture<'static, ()>) {
    let driver = TokioDriver::new();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(15), test(driver.clone()))
                .await
                .unwrap();
        }));
}

#[test]
fn real_context_cancel_reaches_js_signal_and_js_cancel_reaches_running_rust() {
    run(|driver| {
        scenario_real_context_cancel_reaches_js_signal_and_js_cancel_reaches_running_rust(driver)
            .boxed_local()
    });
}

async fn scenario_real_context_cancel_reaches_js_signal_and_js_cancel_reaches_running_rust(
    driver: Rc<TokioDriver>,
) {
    let host = Host::new(
        HostOptions::new("cancellation", json!("host")),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();

    let rust = token("rust");
    let start = token("start");
    let ready = token("ready");
    let foreign = token("foreign");
    let to_rust = token("to-rust");

    let (send, receive) = oneshot::channel();
    let started = Rc::new(RefCell::new(Some(send)));

    host.serve(
        start.clone(),
        Rc::new(move |_, _, _| {
            let started = started.clone();
            Ok(Implementation::new().json_method("run", move |_, _| {
                if let Some(send) = started.borrow_mut().take() {
                    send.send(()).unwrap();
                }
                async { Ok(Some(json!(1))) }.boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();

    let (rust_started, wait_rust) = oneshot::channel();
    let rust_started = Rc::new(RefCell::new(Some(rust_started)));
    let wait_rust = Rc::new(RefCell::new(Some(wait_rust)));

    host.serve(
        ready.clone(),
        Rc::new(move |_, _, _| {
            let wait = wait_rust.clone();
            Ok(Implementation::new().json_method("run", move |_, _| {
                let wait = wait.borrow_mut().take().unwrap();
                async move {
                    wait.await.unwrap();
                    Ok(Some(json!(1)))
                }
                .boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();

    host.serve(
        rust.clone(),
        Rc::new(move |_, _, _| {
            let started = rust_started.clone();
            Ok(Implementation::new().json_method("run", move |_, ctx| {
                let started = started.borrow_mut().take().unwrap();
                async move {
                    started.send(()).unwrap();

                    let reason = ctx.cancellation().cancelled().await;
                    assert_eq!(reason.reason, "from JS");
                    Ok(Some(json!(31)))
                }
                .boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();

    let source = format!(
        r#"
const start={},rust={},foreign={},toRust={},ready={};
export default {{
    name:'cancellation',uses:[],setup(scope,d){{
        d.serve(foreign,(input,self)=>{{
            const heldStart=self.get(start,{{
            }}
            );
            return {{
                methods:{{
                    async run(ctx){{
                        const signal=ctx.signal;
                        await heldStart.run(ctx);
                        await signal.cancelled;
                        if(!signal.aborted||ctx.signal!==signal)throw Error('signal not cancelled');
                        return 17;
                    }}
                }}
                ,state:{{
                }}
            }}
            ;
        }}
        );
        d.serve(toRust,(input,self)=>{{
            const dependency=self.get(rust,{{
            }}
            ),started=self.get(ready,{{
            }}
            );
            return {{
                methods:{{
                    async run(ctx){{
                        const pending=dependency.run(ctx);
                        await started.run(ctx);
                        ctx.cancel('from JS');
                        return await pending;
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
        serde_json::to_string(start.descriptor()).unwrap(),
        serde_json::to_string(rust.descriptor()).unwrap(),
        serde_json::to_string(foreign.descriptor()).unwrap(),
        serde_json::to_string(to_rust.descriptor()).unwrap(),
        serde_json::to_string(ready.descriptor()).unwrap()
    );

    let facet = executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("cancellation", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();
    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();

    let reference = host
        .get(foreign, json!({}), GetOptions::default())
        .await
        .unwrap();

    let captured = Rc::new(RefCell::new(None));
    let context = captured.clone();

    let call = host.root(move |ctx| {
        *context.borrow_mut() = Some(ctx.clone());
        async move { reference.call_json("run", None, Some(&ctx)).await }.boxed_local()
    });

    futures::pin_mut!(call);
    futures::future::poll_fn(|cx| {
        assert!(call.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;

    receive.await.unwrap();
    captured.borrow().as_ref().unwrap().cancel("from Rust");
    assert_eq!(call.await.unwrap(), Some(json!(17)));

    let reference = host
        .get(to_rust, json!({}), GetOptions::default())
        .await
        .unwrap();
    assert_eq!(
        reference.call_json("run", None, None).await.unwrap(),
        Some(json!(31))
    );
    reference.release();

    captured.borrow_mut().take();
    host.close().await.unwrap();
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().contexts, 0);
    assert_eq!(executor.stats().callbacks, 0);
}
