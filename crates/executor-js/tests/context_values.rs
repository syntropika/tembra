use futures::FutureExt;
use std::rc::Rc;
use tembra::{Host, Orb, Service, core::ContextKey};
use tembra_executor_js::{Runtime, TokioExecutor};

#[path = "context_values/storage.rs"]
mod storage;

const TRACE: ContextKey<Rc<str>> = ContextKey::new("bench/trace", 1);

#[test]
fn javascript_rust_contexts_preserve_branches_and_owner_revocation() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(
        &executor,
        async {
            let native = Host::new(TokioExecutor::new());
            native
                .install(vec![Orb::new("probe", 1).service(
                    Service::new("probe", |_| Ok(())).method(
                        "trace",
                        |_, ctx, _: Vec<serde_json::Value>| async move {
                            Ok(ctx.value(&TRACE)?.map(|value| value.to_string()))
                        },
                    ),
                )])
                .unwrap();
            storage::install(&native);
            let sdk = include_str!("../../../packages/ts/sdk/dist/embedded.js");
            let mut runtime = Runtime::new(native, sdk).unwrap();
            runtime
                .evaluate("(async () => { globalThis.probe = await host.get('probe', 'main'); })()")
                .await
                .unwrap();
            let cases = runtime
                .evaluate(include_str!("context_values/scopes.js"))
                .await
                .unwrap();
            assert_eq!(cases.as_array().unwrap().len(), 4);
            runtime.evaluate("probe.release()").await.unwrap();
            assert_eq!(runtime.native().kernel().context_stats().0, 0);
            runtime.close().await.unwrap();
            assert_eq!(runtime.native().kernel().diagnostics().objects, 0);
        }
        .boxed_local(),
    );
}

#[test]
fn mixed_context_frames_reclaim_and_root_issued_opaque_sources_remain_usable() {
    use tembra::core::{ContextKey, OpaqueContextValue};
    const TRACE: ContextKey<String> = ContextKey::new("mixed/trace", 1);
    const SCOPE: ContextKey<OpaqueContextValue> = ContextKey::new("mixed/scope", 1);
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(
        &executor,
        async {
            let host = Host::new(TokioExecutor::new());
            let issued_by = host.clone();
            host.install(vec![
                Orb::new("native.context", 1).service(
                    Service::new("native.context", move |_| Ok(issued_by.clone()))
                        .method("read", |_, ctx, _: Vec<serde_json::Value>| async move {
                            let value = ctx.value(&TRACE)?.unwrap();
                            let derived = ctx.with(&TRACE, "native".into())?;
                            assert_eq!(derived.value(&TRACE)?, Some("native".into()));
                            assert_eq!(ctx.value(&TRACE)?, Some("javascript".into()));
                            Ok(value)
                        })
                        .method("issue", |host, ctx, _: Vec<serde_json::Value>| async move {
                            // Explicitly root-issued: exported source identities must outlive this call.
                            let root = host.context(ctx.root, None)?;
                            Ok(root
                                .with(&SCOPE, OpaqueContextValue::new(77, Rc::new(())))?
                                .frame())
                        })
                        .method("check", |_, ctx, _: Vec<serde_json::Value>| async move {
                            Ok(ctx.value(&SCOPE)?.unwrap().id)
                        })
                        .method(
                            "shortSource",
                            |_, ctx, _: Vec<serde_json::Value>| async move {
                                Ok(ctx.with(&SCOPE, OpaqueContextValue::new(88, ()))?.frame())
                            },
                        ),
                ),
            ])
            .unwrap();
            let mut runtime = Runtime::new(
                host,
                include_str!("../../../packages/ts/sdk/dist/embedded.js"),
            )
            .unwrap();
            let result = runtime
                .evaluate(
                    r#"(async () => {
          const Trace = Tembra.ContextKey.text('mixed/trace');
          const Scope = Tembra.ContextKey.opaque('mixed/scope');
          const native = await host.get('native.context', 'main');
          await host.install({name:'mixed.consumer', apiVersion:1, services:{consumer:{methods:{
            async run(_env, ctx) {
              const framed = ctx.with(Trace, 'javascript');
              const value = await native.read(framed);
              if (value !== 'javascript') throw Error('lost native propagation');
              return value;
            },
            async readOpaque(_env, ctx, token) {
              return await native.check(ctx.with(Scope, token));
            }
          }}}});
          const consumer = await host.get('consumer', 'main');
          const stats = () => host.session.kernel.command('context-stats');
          let peak;
          await host.root(async ctx => {
            const source = await native.issue(ctx);
            const token = Tembra.opaqueFrom(ctx, Scope, source);
            for (let n=0; n<6000; n++) {
              if (await consumer.run(ctx) !== 'javascript') throw Error('wrong result');
              if (await consumer.readOpaque(token, ctx) !== 77) throw Error('wrong owner');
              if (stats().live !== 1) throw Error('frame retention grew');
            }
            const expiredSource = await native['shortSource'](ctx);
            let rejected = false;
            try { Tembra.opaqueFrom(ctx, Scope, expiredSource); } catch { rejected = true; }
            if (!rejected) throw Error('expired invocation source survived');
            peak = stats().peak;
          });
          consumer.release(); native.release();
          return {live:stats().live, peak};
        })()"#,
                )
                .await
                .unwrap();
            assert_eq!(result, serde_json::json!({"live":0,"peak":3}));
            runtime.close().await.unwrap();
        }
        .boxed_local(),
    );
}
