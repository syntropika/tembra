use futures::FutureExt;
use std::rc::Rc;
use tembra::{Facet, Host, Service, core::ContextKey};
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
            let native = Host::new(TokioExecutor);
            native
                .install(vec![Facet::new("probe", 1).service(
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
