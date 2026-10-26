//! Regression cases discovered by independent review of the native facade.
use futures::{FutureExt, task::LocalSpawnExt};
use serde_json::{Value, json};
use std::{rc::Rc, time::Duration};
use tembra::{Host, Orb, Service, core::Requirement};
mod support;
use support::Execution;

const SDK: &str = include_str!("../../../packages/ts/sdk/dist/embedded.js");

fn foreign_requirement() -> Requirement {
    Requirement {
        orb: "js".into(),
        api_version: 1,
        methods: [("js".into(), ["read".into(), "write".into()].into())].into(),
        hooks: Default::default(),
    }
}

#[cfg(feature = "views")]
#[test]
fn foreign_access_applies_to_native_claims_and_reused_objects() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio, async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            let mut runtime = execution.runtime(native.clone(), SDK).unwrap();
            runtime.evaluate("host.install({name:'js',apiVersion:1,services:{js:{access:p=>({methods:p==='reader'?['read']:['read','write']}),methods:{read:()=> 'read',write:()=> 'written'}}}})").await.unwrap();
            native.install(vec![Orb::new("native",1).requires(foreign_requirement())
                .service(Service::new("native",|d|d.get("js",vec!["main".into()]))
                    .method("read",|r,c,_:Vec<Value>|async move{r.call_portable::<_,String>(&c,"read",Vec::<Value>::new()).await})
                    .method("write",|r,c,_:Vec<Value>|async move{r.call_portable::<_,String>(&c,"write",Vec::<Value>::new()).await}))]).unwrap();
            let result=runtime.evaluate("(async()=>{const r=await host.get('native','main',{principal:'reader'});try{let error;try{await r.write()}catch(e){error=e.code}return [await r.read(),error]}finally{r.release()}})()").await.unwrap();
            assert_eq!(result,json!(["read","forbidden"]));
            runtime.close().await.unwrap();
        });
    }
}

#[test]
fn invalid_callback_values_settle_without_poisoning_the_receiver() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio, async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            let mut runtime = execution.runtime(native.clone(), SDK).unwrap();
            runtime.evaluate("host.install({name:'js',apiVersion:1,services:{js:{methods:{read:(_e,_c,v)=>v,write:()=>null}}}})").await.unwrap();
            native.install(vec![Orb::new("native",1).requires(foreign_requirement())
                .service(Service::new("native",|d|d.get("js",vec!["main".into()]))
                    .method("read",|r,c,(bad,):(bool,)|async move{r.call_portable::<_,Value>(&c,"read",vec![if bad {json!(1_u64<<60)}else{json!(42)}]).await}))]).unwrap();
            let result=tokio::time::timeout(Duration::from_secs(1),runtime.evaluate("(async()=>{const r=await host.get('native','main');try{let reason;try{await r.read(true)}catch(e){reason=e.reason}return [reason,await r.read(false)]}finally{r.release()}})()")).await.expect("the callback receiver stopped").unwrap();
            assert_eq!(result,json!(["json_number",42]));
            tokio::time::timeout(Duration::from_secs(1),runtime.close()).await.unwrap().unwrap();
            assert_eq!(native.kernel().diagnostics().calls,0);
        });
    }
}

#[test]
fn unresolved_javascript_promises_report_an_error_without_idle_pump_liveness() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio, async {
            let execution = Execution::new(cooperative);
            let mut runtime = execution
                .runtime(Host::new(execution.clone()), SDK)
                .unwrap();
            let result = tokio::time::timeout(
                Duration::from_millis(100),
                runtime.evaluate("new Promise(()=>{})"),
            )
            .await
            .expect("idle infrastructure kept V8 alive");
            assert!(
                result
                    .unwrap_err()
                    .message
                    .contains("Promise resolution is still pending")
            );
            runtime.close().await.unwrap();
        });
    }
}

#[test]
fn rust_roots_release_javascript_claims_and_controllers() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio,async {
            let execution=Execution::new(cooperative);let native=Host::new(execution.clone());
            let mut runtime=execution.runtime(native.clone(),SDK).unwrap();
            runtime.evaluate("host.install({name:'js',apiVersion:1,services:{js:{methods:{read:async(_e,ctx)=>{ctx.signal;await ctx.get('other','main');return 42},write:()=>null}},other:{methods:{read:()=>1}}}})").await.unwrap();
            let reference=Rc::new(native.get("js",vec!["main".into()],"alice").unwrap());
            let host=native.clone();let r=reference.clone();
            let (send,mut receive)=futures::channel::oneshot::channel();
            execution.spawn_local(async move {
                let result=host.root("alice",move|ctx|async move{r.call_portable::<_,u32>(&ctx,"read",Vec::<Value>::new()).await}.boxed_local()).await;
                send.send(result).unwrap();
            }).unwrap();
            // Keep V8 driven until the native root has returned and its control notification drains.
            let result = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    runtime.evaluate("new Promise(resolve=>setTimeout(()=>resolve(null),1))").await.unwrap();
                    if let Some(result) = receive.try_recv().unwrap() { break result; }
                }
            }).await.unwrap();
            assert_eq!(result.unwrap(),42);
            runtime.evaluate("new Promise(resolve=>setTimeout(()=>resolve(null),1))").await.unwrap();
            assert_eq!(runtime.evaluate("[host.session.rootReferences.size,host.session.controllers.size]").await.unwrap(),json!([0,0]));
            reference.release().unwrap();runtime.close().await.unwrap();
        });
    }
}
