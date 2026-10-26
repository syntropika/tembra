#![cfg(feature = "hooks")]

use futures::FutureExt;
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use tembra::{Hook, Host, Orb, Service, core::ContextKey};

mod support;
use support::Execution;

const TRACE: ContextKey<String> = ContextKey::new("waterfall/trace", 1);
const SDK: &str = include_str!("../../../packages/ts/sdk/dist/embedded.js");

#[test]
fn native_waterfall_policy_stops_mixed_async_contributions_without_changing_bridge_errors() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio, async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            let hook = Hook::<Value, Value>::waterfall_until("prepare", |value| {
                value["decision"] == "Block"
            });
            let ordinary = Hook::<Value, Value>::waterfall("ordinary");
            let decode = Hook::<String, String>::waterfall_until("decode", |_| false);
            let (declared, regular, decoded) = (hook.clone(), ordinary.clone(), decode.clone());
            let late = Rc::new(RefCell::new(Vec::new()));
            let seen = late.clone();
            native.install(vec![Orb::new("native", 1)
                .hook(hook.clone()).hook(ordinary.clone()).hook(decode)
                .contribute("native", &hook, -10, |ctx, mut input| async move {
                    assert_eq!(ctx.principal()?, "alice");
                    assert_eq!(ctx.value(&TRACE)?, Some("carried".into()));
                    input["arguments"] = json!({"path": "rewritten"});
                    Ok(input)
                })
                .contribute("native", &hook, 10, move |_, input| {
                    seen.borrow_mut().push(input.clone());
                    async { Ok(input) }
                })
                .service(Service::new("native", move |self_| {
                    Ok((self_.hook(&declared)?, self_.hook(&regular)?, self_.hook(&decoded)?))
                })
                    .method("run", |hooks, ctx, (input,): (Value,)| async move {
                        hooks.0.run(&ctx, input).await
                    })
                    .method("ordinary", |hooks, ctx, (input,): (Value,)| async move {
                        hooks.1.run(&ctx, input).await
                    })
                    .method("decode", |hooks, ctx, (input,): (String,)| async move {
                        hooks.2.run(&ctx, input).await
                    }))
            ]).unwrap();
            let mut runtime = execution.runtime(native.clone(), SDK).unwrap();
            runtime.evaluate(r#"host.install({name:'js',apiVersion:1,
              requires:{native:{apiVersion:1,hooks:['prepare','ordinary','decode']}},
              contributes:{native:{
                prepare:{priority:0,async handle(ctx,value){
                  if(ctx.principal!=='alice'||ctx.value(Tembra.ContextKey.text('waterfall/trace'))!=='carried')
                    throw Error('lost context');
                  if(value.arguments.path!=='rewritten')throw Error('lost transformation');
                  await new Promise(resolve=>setTimeout(resolve,1));
                  if(value.mode==='error')throw Error('participant failed');
                  return {...value,decision:'Block',reason:'policy'};
                }},
                ordinary:()=>({decision:'Block',reason:'ordinary result'}),
                decode:()=>({invalid:'string result'})
              }}
            })"#).await.unwrap();
            runtime.evaluate(r#"host.install({name:'late',apiVersion:1,
              requires:{native:{apiVersion:1,hooks:['prepare','ordinary']}},
              contributes:{native:{prepare:{priority:20,handle(){throw Error('late participant ran');}},
                ordinary:{priority:20,handle:(ctx,value)=>value}}}
            })"#).await.unwrap();
            // Ordinary waterfall must continue even after a value resembles a terminal decision.
            runtime.evaluate(r#"host.install({name:'last',apiVersion:1,
              requires:{native:{apiVersion:1,hooks:['ordinary']}},
              contributes:{native:{ordinary:{priority:30,handle:(ctx,value)=>({...value,continued:true})}}}
            })"#).await.unwrap();
            let result = runtime.evaluate(r#"(async()=>{
              const reference=await host.get('native','main',{principal:'alice'});
              try{return await host.root(async ctx=>{
                ctx=ctx.with(Tembra.ContextKey.text('waterfall/trace'),'carried');
                const blocked=await reference.run({arguments:{path:'original'}},ctx);
                const initial={decision:'Block',reason:'initial',arguments:{path:'original'}};
                const terminal=await reference.run(initial,ctx);
                let failure,decode,input;
                try{await reference.run({mode:'error'},ctx);}catch(error){failure=String(error);}
                try{await reference.decode('valid',ctx);}catch(error){decode=error.reason;}
                try{await reference.decode({},ctx);}catch(error){input=error.reason;}
                return {blocked,terminal,failure,decode,input,
                  ordinary:await reference.ordinary({},ctx)};
              },{principal:'alice'});}finally{reference.release();}
            })()"#).await.unwrap();
            assert_eq!(result["blocked"], json!({"arguments":{"path":"rewritten"},"decision":"Block","reason":"policy"}));
            assert_eq!(result["terminal"]["arguments"]["path"], "original");
            assert!(result["failure"].as_str().unwrap().contains("participant failed"));
            assert_eq!(result["decode"], "result");
            assert_eq!(result["input"], "arguments");
            assert_eq!(result["ordinary"]["continued"], true);
            assert!(late.borrow().is_empty());
            assert_eq!(native.kernel().context_stats().0, 0);
            runtime.close().await.unwrap();
            assert_eq!(native.kernel().diagnostics().calls, 0);
        }.boxed_local());
    }
}
