//! Foreign claims use the provider policy at acquisition and replacement.
#![cfg(feature = "views")]
use futures::FutureExt;
use serde_json::{Value, json};
use std::{rc::Rc, time::Duration};
use tembra::{AcquisitionOptions, Host, Orb, Service, core::Requirement};
mod support;
use support::Execution;
const SDK: &str = include_str!("../../../packages/ts/sdk/dist/embedded.js");

fn install_forwarder(native: &Host) {
    native
        .install(vec![
            Orb::new("native", 1)
                .requires(Requirement {
                    orb: "js".into(),
                    api_version: 1,
                    methods: [("js".into(), ["read".into(), "write".into()].into())].into(),
                    hooks: Default::default(),
                })
                .service(
                    Service::new("native", |declaration| {
                        declaration.get("js", vec!["main".into()])
                    })
                    .method("read", |dependency, ctx, _: Vec<Value>| async move {
                        dependency
                            .call_portable::<_, String>(&ctx, "read", Vec::<Value>::new())
                            .await
                    })
                    .method("write", |dependency, ctx, _: Vec<Value>| async move {
                        dependency
                            .call_portable::<_, String>(&ctx, "write", Vec::<Value>::new())
                            .await
                    })
                    .method("dynamic", |_, ctx, _: Vec<Value>| async move {
                        let claim =
                            ctx.get("js", vec!["main".into()], AcquisitionOptions::default())?;
                        claim
                            .call_portable::<_, String>(&ctx, "write", Vec::<Value>::new())
                            .await
                    }),
                ),
        ])
        .unwrap();
}

#[test]
fn reused_native_claims_and_context_acquisitions_obey_foreign_views() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio, async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            let mut runtime = execution.runtime(native.clone(), SDK).unwrap();
            runtime.evaluate("(async()=>{await host.install({name:'js',apiVersion:1,services:{js:{access:p=>({methods:p==='reader'?['read']:['read','write']}),methods:{read:()=> 'read',write:()=> 'written'}}}});globalThis.direct=await host.get('js','main',{principal:'reader'});})()").await.unwrap();
            let independent = Rc::new(native.get_with_options("js", vec!["main".into()], "reader", AcquisitionOptions { api_version: 1, methods: ["read".into(), "write".into()].into() }).unwrap());
            install_forwarder(&native);
            let (send, receive) = futures::channel::oneshot::channel();
            let claim = independent.clone();
            let host = native.clone();
            use futures::task::LocalSpawnExt;
            execution.spawn_local(async move {
                let result = host.root("reader", move |ctx| async move {
                    claim.call_portable::<_, String>(&ctx, "write", Vec::<Value>::new()).await
                }.boxed_local()).await;
                send.send(result).unwrap();
            }).unwrap();
            let result = runtime.evaluate("(async()=>{const reader=await host.get('native','main',{principal:'reader'});const admin=await host.get('native','main',{principal:'admin'});try{let errors=[];for(const method of ['write','dynamic'])try{await reader[method]();errors.push('allowed')}catch(e){errors.push(e.code)}return [await reader.read(),...errors,await admin.write()]}finally{reader.release();admin.release();direct.release()}})()").await.unwrap();
            assert_eq!(result, json!(["read", "forbidden", "forbidden", "written"]));
            assert_eq!(receive.await.unwrap().unwrap_err().code, tembra::Code::Forbidden);
            independent.release().unwrap();
            runtime.close().await.unwrap();
        });
    }
}

#[test]
fn native_claim_policies_refresh_on_reload_and_recover_after_policy_errors() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio, async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            let mut runtime = execution.runtime(native.clone(), SDK).unwrap();
            runtime.evaluate("globalThis.policy=['read','write'];globalThis.definition=()=>({name:'js',apiVersion:1,services:{js:{access:()=>({methods:policy}),methods:{read:()=> 'read',write:()=> 'written'}}}});host.install(definition())").await.unwrap();
            install_forwarder(&native);
            let result = runtime.evaluate("(async()=>{const ref=await host.get('native','main',{principal:'reader'});try{const initial=await ref.write();policy=['read'];const cached=await ref.write();await host.reload([definition()]);let tightened;try{await ref.write()}catch(e){tightened=e.code}const broken=definition();broken.services.js.access=()=>{throw Error('policy failed')};await host.reload([broken]);let failed;try{await ref.read()}catch(e){failed=e.code}policy=['read','write'];await host.reload([definition()]);return [initial,cached,tightened,failed,await ref.write()]}finally{ref.release()}})()").await.unwrap();
            assert_eq!(result, json!(["written", "written", "forbidden", "forbidden", "written"]));
            assert_eq!(runtime.native().kernel().diagnostics().calls, 0);
            runtime.close().await.unwrap();
        });
    }
}

#[test]
fn native_claim_release_before_activation_does_not_retain_a_js_record() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio, async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            let mut runtime = execution.runtime(native.clone(), SDK).unwrap();
            runtime.evaluate("host.install({name:'js',apiVersion:1,services:{js:{access:()=>({methods:[]}),methods:{read:()=>42}}}})").await.unwrap();
            let reference = native.get("js", vec!["main".into()], "reader").unwrap();
            reference.release().unwrap();
            tokio::time::timeout(Duration::from_secs(1), runtime.evaluate("new Promise(resolve=>setTimeout(()=>resolve(null),10))")).await.unwrap().unwrap();
            assert_eq!(runtime.evaluate("host.session.references.size").await.unwrap(), json!(0));
            runtime.close().await.unwrap();
        });
    }
}
