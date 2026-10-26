use futures::{FutureExt, channel::oneshot};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};
use tembra::{Host, Orb, Service, core::ContextKey};
mod support;
use support::Execution;
const TRACE: ContextKey<String> = ContextKey::new("settlement/trace", 1);
#[test]
fn observer_cancellation_keeps_native_work_and_resources_alive_until_physical_settlement() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio,async{
        let execution=Execution::new(cooperative);
        let native=Host::new(execution.clone());
        let (send,receive)=oneshot::channel::<()>();
        let receive=Rc::new(RefCell::new(Some(receive)));
        let entered=Rc::new(Cell::new(false));let cancelled=Rc::new(Cell::new(false));let closed=Rc::new(Cell::new(false));
        let e=entered.clone();let c=cancelled.clone();let d=closed.clone();
        native.install(vec![Orb::new("slow",1).service(Service::new("slow",move|declaration|{
            let d=d.clone();
            declaration.resource("lifetime",1,||Ok(()),move|_,_|{let d=d.clone();async move{d.set(true);Ok(())}.boxed_local()})?;
            Ok((receive.clone(),e.clone(),c.clone()))
        }).method("wait",|state,ctx,_:Vec<serde_json::Value>|async move{
            assert_eq!(ctx.principal()?,"alice");
            assert_eq!(ctx.value(&TRACE)?,Some("carried".into()));
            let receive=state.0.borrow_mut().take().unwrap();state.1.set(true);
            ctx.signal().cancelled().await;state.2.set(true);
            receive.await.unwrap();Ok(42_u32)
        }))]).unwrap();
        let mut runtime=execution.runtime(native.clone(),include_str!("../../../packages/ts/sdk/dist/embedded.js")).unwrap();
        runtime.evaluate(r#"(async()=>{globalThis.slow=await host.get('slow','main',{principal:'alice'});})()"#).await.unwrap();
        runtime.evaluate(r#"globalThis.abandoned=host.root(ctx=>{ctx=ctx.with(Tembra.ContextKey.text('settlement/trace'),'carried');globalThis.waitContext=ctx;return slow.wait(ctx);},{principal:'alice'});abandoned.catch(()=>{});null"#).await.unwrap();
        runtime.evaluate("new Promise(resolve=>setTimeout(()=>resolve(null),1))").await.unwrap();
        assert!(entered.get());assert_eq!(native.kernel().diagnostics().calls,1);
        assert_eq!(runtime.evaluate("waitContext.cancel();abandoned.then(()=>false,e=>e.code==='cancelled')").await.unwrap(),serde_json::json!(true));
        runtime.evaluate("new Promise(resolve=>setTimeout(()=>resolve(null),1))").await.unwrap();
        assert!(cancelled.get());assert!(!closed.get());assert_eq!(native.kernel().diagnostics().calls,1);
        let d=closed.clone();let host=native.clone();
        tokio::task::spawn_local(async move{
            tokio::time::sleep(Duration::from_millis(10)).await;
            assert!(!d.get());assert_eq!(host.kernel().diagnostics().calls,1);send.send(()).unwrap();
        });
        runtime.close().await.unwrap();
        assert!(closed.get());assert_eq!(native.kernel().diagnostics().calls,0);assert_eq!(native.kernel().diagnostics().objects,0);
    }.boxed_local());
    }
}

#[test]
fn dropping_the_rust_wait_keeps_the_callback_running_between_evaluations() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(
            &tokio,
            async {
                let execution = Execution::new(cooperative);
                let native = Host::new(execution.clone());
                let (send, receive) = oneshot::channel::<()>();
                let receive = Rc::new(RefCell::new(Some(receive)));
                let started = Rc::new(Cell::new(false));
                let finished = Rc::new(Cell::new(false));
                let closed = Rc::new(Cell::new(false));
                let s = started.clone();
                let f = finished.clone();
                let c = closed.clone();
                native
                    .install(vec![
                        Orb::new("held", 1).service(
                            Service::new("held", move |declaration| {
                                let c = c.clone();
                                declaration.resource(
                                    "resource",
                                    1,
                                    || Ok(()),
                                    move |_, _| {
                                        let c = c.clone();
                                        async move {
                                            c.set(true);
                                            Ok(())
                                        }
                                        .boxed_local()
                                    },
                                )?;
                                Ok((receive.clone(), s.clone(), f.clone()))
                            })
                            .method(
                                "wait",
                                |state, _, _: Vec<serde_json::Value>| async move {
                                    let receive = state.0.borrow_mut().take().unwrap();
                                    state.1.set(true);
                                    receive.await.unwrap();
                                    state.2.set(true);
                                    Ok(42)
                                },
                            ),
                        ),
                    ])
                    .unwrap();
                let mut runtime = execution
                    .runtime(
                        native.clone(),
                        include_str!("../../../packages/ts/sdk/dist/embedded.js"),
                    )
                    .unwrap();
                runtime
                    .evaluate("(async()=>{globalThis.held=await host.get('held','main');})()")
                    .await
                    .unwrap();
                assert!(
                    tokio::time::timeout(
                        Duration::from_millis(10),
                        runtime.evaluate("held.wait()")
                    )
                    .await
                    .is_err()
                );
                assert!(started.get());
                assert!(!finished.get());
                assert!(!closed.get());
                assert_eq!(native.kernel().diagnostics().calls, 1);
                send.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(1), native.settled())
                    .await
                    .unwrap();
                assert!(finished.get());
                assert!(!closed.get());
                runtime.close().await.unwrap();
                assert!(closed.get());
                assert_eq!(native.kernel().diagnostics().calls, 0);
                assert_eq!(native.kernel().diagnostics().objects, 0);
            }
            .boxed_local(),
        );
    }
}
