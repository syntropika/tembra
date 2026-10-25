use futures::FutureExt;
use serde_json::json;
use tembra::{Facet, Host, Service};
use tembra_executor_js::{Runtime, TokioExecutor};

fn sdk_source() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packages/sdk/dist/embedded.js"
    ))
    .expect("build the SDK before running embedded integration tests")
}

#[test]
fn javascript_provider_moves_preserve_state_and_candidate_dependencies() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(
        &executor,
        async {
            let mut runtime = Runtime::new(Host::new(TokioExecutor), sdk_source()).unwrap();
            let result = runtime
                .evaluate(
                    r#"(async () => {
          const provider = (name, serves) => ({name, apiVersion: 1, services: serves ? {item: {
            create(self) { return {state: self.state(0)}; },
            methods: {
              read: ({state}) => [state.value, name],
              increment: ({state}, ctx) => state.update(ctx, value => value + 1)
            }
          }} : {}});
          const consumer = facet => ({name: 'consumer', apiVersion: 1,
            requires: {source: {facet, apiVersion: 1, methods: ['read']}},
            services: {client: {
              create(self) {
                const source = self.get('source', 'main');
                self.resource('cache', {deps: [source], open: () => null,
                  close: (_value, ctx) => source.read(ctx)});
                return {source};
              },
              methods: {read: ({source}, ctx) => source.read(ctx)}
            }}
          });
          await host.install([provider('a', true), consumer('a')]);
          const reference = await host.get('item', 'main');
          const client = await host.get('client', 'main');
          await reference.increment();
          await host.reload([provider('a', false), provider('b', true), consumer('b')]);
          const result = await client.read();
          reference.release(); client.release();
          return result;
        })()"#,
                )
                .await
                .unwrap();
            assert_eq!(result, json!([1, "b"]));
            runtime.close().await.unwrap();
            assert_eq!(runtime.native().kernel().diagnostics().objects, 0);
        }
        .boxed_local(),
    );
}

#[test]
fn javascript_uses_native_provider_through_one_kernel() {
    let source = sdk_source();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&executor, async {
        let native = Host::new(TokioExecutor);
        native.install(vec![Facet::new("storage", 1).service(Service::new("storage", |self_| self_.state(0_i32, 1, |_| Ok(()))).method("add", |state, context, (amount,): (i32,)| async move { Ok(*state.update(&context, |value| Ok(value + amount))?) }))]).unwrap();
        let mut runtime = Runtime::new(native, source).unwrap();
        let id = runtime.native().kernel().installed_facets()[0].id;
        let precision = runtime.evaluate(&format!(r#"(() => {{
          const has = Deno.core.ops.op_tembra_has_facet;
          const invalid = [2 ** 53, -1, 1.5, NaN, Infinity].map(id => {{ try {{ has(id); return false; }} catch {{ return true; }} }});
          return [has({id}), has({wide}), has(Number.MAX_SAFE_INTEGER), ...invalid];
        }})()"#, wide = id + (1_u64 << 32))).await.unwrap();
        assert_eq!(precision, json!([true, false, false, true, true, true, true, true]));
        runtime.install("client", r#"globalThis.__tembraFacet = { name: 'client', apiVersion: 1, requires: { storage: { apiVersion: 1, methods: ['add'] } }, services: { client: {
          create(self) { return { storage: self.get('storage', 'main') }; },
          methods: { async run({storage}, ctx, value) { await new Promise(resolve => setTimeout(resolve, 1)); return storage.add(value, ctx); } }
        } } };"#.into()).await.unwrap();
        let result = runtime.evaluate("(async () => { globalThis.client = await host.get('client', 'main'); return await client.run(7); })()").await.unwrap();
        assert_eq!(result, json!(7));
        let result = runtime.evaluate("(async () => { const storage = await host.get('storage', 'main'); const before = storage.state.value; await storage.add(3); const result = [before, storage.state.value]; storage.release(); return result; })()").await.unwrap();
        assert_eq!(result, json!([7, 10]));
        runtime.close().await.unwrap();
        assert_eq!(runtime.native().kernel().diagnostics().objects, 0);
    }.boxed_local());
}

#[test]
fn native_state_decode_failure_does_not_replace_the_call_result_or_lose_disposal() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(
        &executor,
        async {
            let native = Host::new(TokioExecutor);
            native
                .install(vec![Facet::new("native", 1).service(
                    Service::new("native", |self_| self_.state(0_u64, 1, |_| Ok(()))).method(
                        "update",
                        |state, context, (power,): (u32,)| async move {
                            tokio::task::yield_now().await;
                            state.update(&context, |_| Ok(1_u64 << power))?;
                            Ok(())
                        },
                    ),
                )])
                .unwrap();
            let mut runtime = Runtime::new(native, sdk_source()).unwrap();
            let result = runtime
                .evaluate(
                    r#"(async () => {
          const reference = await host.get('native', 'main');
          const calling = reference.update(60);
          for (let turn = 0; turn < 8 && !host.diagnostics().calls; turn++) await Promise.resolve();
          if (host.diagnostics().calls !== 1) throw new Error('Native call was not admitted');
          reference.release();
          const value = await calling;
          await host.close();
          return {
            value,
            errors: host.errors().map(({code, reason}) => ({code, reason})),
            objects: host.diagnostics().objects,
            values: host.diagnostics().values
          };
        })()"#,
                )
                .await;
            let closing = runtime.close().await;
            assert_eq!(
                result.unwrap(),
                json!({
                    "value": null, "errors": [{"code": "invalid", "reason": "json_number"}],
                    "objects": 0, "values": 0,
                })
            );
            closing.unwrap();
            assert_eq!(runtime.native().kernel().diagnostics().objects, 0);
        }
        .boxed_local(),
    );
}

#[test]
fn portable_results_copy_the_codec_view_without_iterating_bytes() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&executor, async {
        let mut runtime = Runtime::with_codec(
            Host::new(TokioExecutor),
            sdk_source(),
            std::rc::Rc::new(tembra::codec::Json),
            r#"({
              id: Tembra.jsonCodec.id,
              deserialize: value => Tembra.jsonCodec.deserialize(value),
              serialize(value) {
                const encoded = Tembra.jsonCodec.serialize(value);
                const storage = new Uint8Array(encoded.length + 11).fill(255);
                storage.set(encoded, 7);
                const view = storage.subarray(7, 7 + encoded.length);
                Object.defineProperty(view, Symbol.iterator, {
                  value() { throw new Error('Codec bytes must not be iterated in JavaScript'); }
                });
                return view;
              }
            })"#,
        )
        .unwrap();
        runtime
            .install(
                "results",
                r#"globalThis.__tembraFacet = {
          name: 'results', apiVersion: 1, services: { results: { methods: {
            numbers(_environment, _context, count) {
              return Array.from({ length: count }, (_, index) => index - 100);
            },
            binary(_environment, _context, count) {
              return Uint8Array.from({ length: count }, (_, index) => index % 251);
            }
          } } }
        };"#
                .into(),
            )
            .await
            .unwrap();
        let key = vec!["main".into()];
        let numbers = runtime
            .call("results", &key, "numbers", json!([16_384]))
            .await
            .unwrap();
        assert_eq!(numbers, json!((-100..16_284).collect::<Vec<_>>()));
        let bytes = runtime
            .call("results", &key, "binary", json!([32_768]))
            .await
            .unwrap();
        let expected: Vec<_> = (0..32_768).map(|index| (index % 251) as u8).collect();
        assert_eq!(bytes, tembra::codec::bytes(&expected));
        runtime.close().await.unwrap();
    });
}

#[test]
fn codec_results_support_shared_views_and_refuse_detached_views() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&executor, async {
        for shared in [true, false] {
            let codec = format!(
                r#"({{
              id: Tembra.jsonCodec.id,
              deserialize: value => Tembra.jsonCodec.deserialize(value),
              serialize(value) {{
                const encoded = Tembra.jsonCodec.serialize(value);
                if ({shared}) {{
                  const storage = new Uint8Array(new SharedArrayBuffer(encoded.length + 9));
                  storage.set(encoded, 5);
                  return storage.subarray(5, 5 + encoded.length);
                }}
                encoded.buffer.transfer();
                return encoded;
              }}
            }})"#
            );
            let mut runtime = Runtime::with_codec(
                Host::new(TokioExecutor),
                sdk_source(),
                std::rc::Rc::new(tembra::codec::Json),
                &codec,
            )
            .unwrap();
            runtime
                .install(
                    "result",
                    r#"globalThis.__tembraFacet = {
              name: 'result', apiVersion: 1,
              services: { result: { methods: { read: () => 42 } } }
            };"#
                    .into(),
                )
                .await
                .unwrap();
            let result = runtime
                .call("result", &["main".into()], "read", json!([]))
                .await;
            if shared {
                assert_eq!(result.unwrap(), json!(42));
            } else {
                assert_eq!(result.unwrap_err().reason, "byte_result");
            }
            runtime.close().await.unwrap();
        }
    });
}
