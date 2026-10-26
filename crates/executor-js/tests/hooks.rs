#![cfg(feature = "hooks")]
use futures::FutureExt;
use serde_json::{Value, json};
use tembra::{
    Hook, Host, Orb, Service,
    core::{ContextKey, Requirement},
};
mod support;
use support::Execution;

const TRACE: ContextKey<String> = ContextKey::new("hooks/trace", 1);

#[test]
fn mixed_plans_run_in_both_directions_and_preserve_context_and_undefined() {
    for cooperative in [false, true] {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&executor, async {
let execution = Execution::new(cooperative);
        let native = Host::new(execution.clone());
        let hook = Hook::<String, String>::waterfall("transform");
        let bail = Hook::<String, Option<Value>>::bail("decide");
        let declared = hook.clone();
        let decide = bail.clone();
        native.install(vec![Orb::new("native", 1).hook(hook.clone()).hook(bail.clone())
            .contribute("native", &hook, 0, |ctx, value| async move {
                assert_eq!(ctx.value(&TRACE)?, Some("from-js".into()));
                assert_eq!(ctx.principal()?, "host");
                Ok(format!("{value}:rust"))
            })
            .contribute("native", &hook, 0, |_, value| async move {Ok(format!("{value}:rust2"))})
            .contribute("native", &bail, -1, |_, _| async { Ok(None) })
            .service(Service::new("native", move |self_| Ok((self_.hook(&declared)?, self_.hook(&decide)?)))
                .method("transform", |hooks, ctx, (input,): (String,)| async move { hooks.0.run(&ctx, input).await })
                .method("decide", |hooks, ctx, (input,): (String,)| async move { hooks.1.run(&ctx, input).await }))
        ]).unwrap();
        let mut runtime = execution.runtime(native.clone(), include_str!("../../../packages/ts/sdk/dist/embedded.js")).unwrap();
        runtime.evaluate(r#"(async () => {
          const trace = Tembra.ContextKey.text('hooks/trace');
          await host.install({name:'js',apiVersion:1,
            requires:{native:{apiVersion:1,hooks:['transform','decide']}},
            contributes:{native:{
              transform:{priority:0,async handle(ctx,value){
                if(ctx.value(trace)!=='from-js'||ctx.principal!=='host')throw Error('lost context');
                await new Promise(resolve=>setTimeout(resolve,1));
                return value+':js';
              }},
              decide:() => null
            }},
            hooks:{transform:{async run(ctx,value,participants){for(const p of participants)value=await p(ctx,value);return value;}}},
            services:{js:{create(self){return self.hook('transform');},methods:{run:(hook,ctx,value)=>hook.run(ctx,value)}}}
          });
        })()"#).await.unwrap();
        native.install(vec![Orb::new("contributor", 1)
            .requires(Requirement {orb:"js".into(),api_version:1,methods:Default::default(),hooks:["transform".into()].into()})
            .contribute("js", &hook, 0, |ctx, value| async move {
                assert_eq!(ctx.value(&TRACE)?, Some("from-js".into()));
                Ok(format!("{value}:native"))
            })]).unwrap();
        let result = runtime.evaluate(r#"(async()=>{
          const trace=Tembra.ContextKey.text('hooks/trace');
          const rust=await host.get('native','main'); const js=await host.get('js','main');
          try{return await host.root(async ctx=>{
            ctx=ctx.with(trace,'from-js');
            return [await rust.transform('start',ctx),await js.run('start',ctx),await rust.decide('start',ctx)];
          });}finally{rust.release();js.release();}
        })()"#).await.unwrap();
        assert_eq!(result,json!(["start:rust:rust2:js","start:native",null]));
        assert_eq!(native.kernel().context_stats().0,0);
        runtime.close().await.unwrap();
    }.boxed_local());
    }
}

#[test]
fn javascript_series_without_input_can_invoke_a_native_unit_contribution() {
    for cooperative in [false, true] {
        use std::{cell::Cell, rc::Rc};
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&executor,async{
let execution = Execution::new(cooperative);
        let native=Host::new(execution.clone());let called=Rc::new(Cell::new(false));let observed=called.clone();
        let mut runtime=execution.runtime(native.clone(),include_str!("../../../packages/ts/sdk/dist/embedded.js")).unwrap();
        runtime.evaluate(r#"host.install({name:'js',apiVersion:1,hooks:{observe:{async run(ctx,input,participants){for(const participant of participants)await participant(ctx,input);}}},services:{js:{create(self){return self.hook('observe');},methods:{run:(hook,ctx)=>hook.run(ctx)}}}})"#).await.unwrap();
        native.install(vec![Orb::new("native",1).requires(Requirement{orb:"js".into(),api_version:1,methods:Default::default(),hooks:["observe".into()].into()})
            .contribute("js",&Hook::<(),()>::series("observe"),0,move|_,()|{observed.set(true);async{Ok(())}})]).unwrap();
        runtime.evaluate("(async()=>{const reference=await host.get('js','main');try{await reference.run();return null;}finally{reference.release();}})()").await.unwrap();
        assert!(called.get());runtime.close().await.unwrap();
    }.boxed_local());
    }
}
