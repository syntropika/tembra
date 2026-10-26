use futures::FutureExt;
use serde_json::json;
use tembra::{
    Host, Orb, Service,
    core::{ContextKey, Requirement},
};
mod support;
use support::Execution;

#[test]
fn native_dependencies_call_javascript_with_the_original_context_and_errors() {
    for cooperative in [false, true] {
        const TRACE: ContextKey<String> = ContextKey::new("facade/trace", 1);
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&executor,async{
let execution = Execution::new(cooperative);
        let native=Host::new(execution.clone());
        let mut runtime=execution.runtime(native.clone(),include_str!("../../../packages/ts/sdk/dist/embedded.js")).unwrap();
        runtime.evaluate(r#"host.install({name:'js',apiVersion:1,services:{js:{methods:{
          read(_env,ctx,input){if(ctx.principal!=='alice')throw Error('wrong principal');return [ctx.value(Tembra.ContextKey.text('facade/trace')),input];},
          fail(){throw {code:'invalid',reason:'test_failure',message:'foreign callback failed'};}
        }}}})"#).await.unwrap();
        native.install(vec![Orb::new("native",1)
            .requires(Requirement{orb:"js".into(),api_version:1,methods:[("js".into(),["read".into(),"fail".into()].into())].into(),hooks:Default::default()})
            .service(Service::new("native",|self_|self_.get("js",vec!["main".into()]))
                .method("read",|dependency,ctx,(input,):(String,)|async move{
                    dependency.call_portable::<_,serde_json::Value>(&ctx.with(&TRACE,"native".into())?,"read",(input,)).await
                })
                .method("fail",|dependency,ctx,_:Vec<serde_json::Value>|async move{
                    dependency.call_portable::<_,serde_json::Value>(&ctx,"fail",Vec::<serde_json::Value>::new()).await
                }))]).unwrap();
        let pending=tokio::time::timeout(std::time::Duration::from_secs(2),runtime.evaluate(r#"(async()=>{const native=await host.get('native','main',{principal:'alice'});try{return await native.read('value');}finally{native.release();}})()"#)).await;
        let result=pending.unwrap().unwrap();
        assert_eq!(result,json!(["native","value"]));
        let failure=runtime.evaluate(r#"(async()=>{const native=await host.get('native','main',{principal:'alice'});try{await native.fail();return null;}catch(error){return [error.code,error.reason];}finally{native.release();}})()"#).await.unwrap();
        assert_eq!(failure,json!(["invalid","test_failure"]));
        assert_eq!(native.kernel().context_stats().0,0);
        runtime.close().await.unwrap();assert_eq!(native.kernel().diagnostics().objects,0);
    }.boxed_local());
    }
}

#[cfg(feature = "subscriptions")]
#[test]
fn shared_shutdown_signals_native_events_and_waits_for_physical_completion() {
    for cooperative in [false, true] {
        use futures::channel::oneshot;
        use std::{
            cell::{Cell, RefCell},
            rc::Rc,
        };
        use tembra::{EventHandler, EventOptions, EventSource};
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(
            &executor,
            async {
                let execution = Execution::new(cooperative);
                let native = Host::new(execution.clone());
                let cancelled = Rc::new(Cell::new(false));
                let closed = Rc::new(Cell::new(false));
                let emitter = Rc::new(RefCell::new(None::<EventHandler<i32>>));
                let captured = emitter.clone();
                let source = EventSource::new(move |handler| {
                    *captured.borrow_mut() = Some(handler);
                    let captured = captured.clone();
                    Ok(Box::new(move || {
                        captured.borrow_mut().take();
                    }))
                });
                let (send, receive) = oneshot::channel();
                let receive = Rc::new(RefCell::new(Some(receive)));
                let observed = cancelled.clone();
                let observed_close = closed.clone();
                native
                    .install(vec![Orb::new("events", 1).service(Service::new(
                        "events",
                        move |self_| {
                            let receive = receive.clone();
                            let observed = observed.clone();
                            let closed = observed_close.clone();
                            self_.resource(
                                "resource",
                                1,
                                || Ok(()),
                                move |_, _| {
                                    async move {
                                        closed.set(true);
                                        Ok(())
                                    }
                                    .boxed_local()
                                },
                            )?;
                            self_.on(
                                "events",
                                source.clone(),
                                EventOptions::default(),
                                move |ctx, _| {
                                    let receive = receive.borrow_mut().take().unwrap();
                                    let observed = observed.clone();
                                    async move {
                                        ctx.signal().cancelled().await;
                                        observed.set(true);
                                        receive.await.unwrap();
                                        Ok(())
                                    }
                                },
                            )?;
                            Ok(())
                        },
                    ))])
                    .unwrap();
                let mut runtime = execution
                    .runtime(
                        native.clone(),
                        include_str!("../../../packages/ts/sdk/dist/embedded.js"),
                    )
                    .unwrap();
                runtime
                    .evaluate("(async()=>{globalThis.events=await host.get('events','main');})()")
                    .await
                    .unwrap();
                emitter.borrow().clone().unwrap()(Rc::new(1));
                runtime
                    .evaluate("new Promise(resolve=>setTimeout(()=>resolve(null),1))")
                    .await
                    .unwrap();
                assert_eq!(native.kernel().diagnostics().calls, 1);
                let observed = cancelled.clone();
                let still_open = closed.clone();
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    assert!(observed.get());
                    assert!(!still_open.get());
                    send.send(()).unwrap();
                });
                runtime.close().await.unwrap();
                assert!(closed.get());
                assert_eq!(native.kernel().diagnostics().calls, 0);
            }
            .boxed_local(),
        );
    }
}

#[cfg(feature = "views")]
#[test]
fn native_access_policies_apply_to_javascript_references() {
    for cooperative in [false, true] {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&executor,async{
let execution = Execution::new(cooperative);
        let native=Host::new(execution.clone());
        native.install(vec![Orb::new("restricted",1).service(Service::new("restricted",|_|Ok(()))
            .method("read",|_,_,_:Vec<serde_json::Value>|async{Ok(1)})
            .method("write",|_,_,_:Vec<serde_json::Value>|async{Ok(2)})
            .access(|principal|if principal=="admin"{["read".into(),"write".into()].into()}else{["read".into()].into()}))]).unwrap();
        let mut runtime=execution.runtime(native,include_str!("../../../packages/ts/sdk/dist/embedded.js")).unwrap();
        let result=runtime.evaluate("(async()=>{const reader=await host.get('restricted','main');const admin=await host.get('restricted','main',{principal:'admin'});try{return [typeof reader.write,await reader.read(),await admin.write()];}finally{reader.release();admin.release();}})()").await.unwrap();
        assert_eq!(result,json!(["undefined",1,2]));runtime.close().await.unwrap();
    }.boxed_local());
    }
}

#[test]
fn native_and_javascript_waiters_share_the_same_activation_completion() {
    for cooperative in [false, true] {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&executor,async{
let execution = Execution::new(cooperative);
        let native=Host::new(execution.clone());
        native.install(vec![Orb::new("shared",1).service(Service::new("shared",|_|Ok(()))
            .method("read",|_,_,_:Vec<serde_json::Value>|async{Ok(42)}))]).unwrap();
        let mut runtime=execution.runtime(native.clone(),include_str!("../../../packages/ts/sdk/dist/embedded.js")).unwrap();
        let reference=native.get("shared",vec!["main".into()],"host").unwrap();
        let result=tokio::time::timeout(std::time::Duration::from_secs(2),runtime.evaluate("(async()=>{const reference=await host.get('shared','main');try{return await reference.read();}finally{reference.release();}})()")).await.expect("both activation waiters must resolve").unwrap();
        assert_eq!(result,json!(42));reference.release().unwrap();runtime.close().await.unwrap();
    }.boxed_local());
    }
}

#[test]
fn javascript_root_completion_revokes_native_root_owned_claims_and_orb_storage() {
    for cooperative in [false, true] {
        use std::{
            cell::{Cell, RefCell},
            rc::Rc,
        };
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&executor,async{
let execution = Execution::new(cooperative);
        struct Shared(Rc<Cell<bool>>);
        impl Drop for Shared{fn drop(&mut self){self.0.set(true);}}
        let native=Host::new(execution.clone());let released=Rc::new(Cell::new(false));let captured=released.clone();
        let retained=Rc::new(RefCell::new(None::<Rc<tembra::Reference>>));let captured_reference=retained.clone();
        native.install(vec![Orb::new("issuer",1).create(move|_|Ok(Shared(captured.clone())))
            .service(Service::new("issuer",|_|Ok(())).method("acquire",move|_,ctx,_:Vec<serde_json::Value>|{
                let retained=captured_reference.clone();async move{
                    let reference=ctx.get("issuer",vec!["other".into()],Default::default())?;reference.ready().await?;
                    *retained.borrow_mut()=Some(reference);Ok(())
                }
            }))]).unwrap();
        let mut runtime=execution.runtime(native.clone(),include_str!("../../../packages/ts/sdk/dist/embedded.js")).unwrap();
        runtime.evaluate("(async()=>{const reference=await host.get('issuer','main');try{await reference.acquire();}finally{reference.release();}})()").await.unwrap();
        assert_eq!(retained.borrow().as_ref().unwrap().status().unwrap(),tembra::core::Status::Released);
        runtime.close().await.unwrap();assert!(released.get());
    }.boxed_local());
    }
}
