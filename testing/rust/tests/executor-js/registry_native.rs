use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;
struct Marker;
struct MarkerCodec;
impl ScopedCodec<Rc<Marker>> for MarkerCodec {
    fn encode(&self, value: Rc<Marker>) -> Result<NativeValue> {
        Ok(NativeValue::capability(value))
    }

    fn decode(&self, value: NativeValue) -> Result<Rc<Marker>> {
        value.downcast()
    }
}

struct IntegerCodec;
impl ScopedCodec<i64> for IntegerCodec {
    fn encode(&self, value: i64) -> Result<NativeValue> {
        Ok(NativeValue::portable(json!(value)))
    }

    fn decode(&self, value: NativeValue) -> Result<i64> {
        value
            .into_portable()?
            .as_i64()
            .ok_or_else(|| RuntimeError::invalid("integer", "expected integer"))
    }
}

fn token(name: &str, args: Option<ValueSchema>, result: Option<ValueSchema>) -> Rc<Contract> {
    Rc::new(
        Contract::local(
            name,
            1,
            json!({}),
            BTreeMap::from([("run".into(), Member::Method { args, result })]),
        )
        .unwrap(),
    )
}

#[test]
fn reentrant_registry_tables_dispatch_real_mixed_native_pairs_and_refuse_stale_or_forged_values() {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(
                Duration::from_secs(15),
                check_registry_dispatch(driver.clone()),
            )
            .await
            .unwrap();
        }));
}

async fn check_registry_dispatch(driver: Rc<TokioDriver>) {
    let host = Host::new(
        HostOptions::new("registry", json!("principal")),
        driver.clone(),
    );
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let native = ValueSchema::Native("marker@1".into());
    let portable = ValueSchema::Portable(json!({ "type": "integer" }));
    executor
        .register_native_method(
            NativeMethodSignature::from_schemas(Some(portable.clone()), Some(native.clone()))
                .unwrap(),
            NativeMethodCodec::new(Rc::new(IntegerCodec), Rc::new(MarkerCodec)),
        )
        .unwrap();
    executor
        .register_native_method(
            NativeMethodSignature::from_schemas(Some(native.clone()), Some(portable.clone()))
                .unwrap(),
            NativeMethodCodec::new(Rc::new(MarkerCodec), Rc::new(IntegerCodec)),
        )
        .unwrap();
    let registry = Registry::new("operations", ConflictPolicy::Error).unwrap();
    let marker = Rc::new(Marker);
    let effects = Rc::new(Cell::new(0));
    let issue = token("issue", Some(portable.clone()), Some(native.clone()));
    let expected = marker.clone();
    let calls = effects.clone();
    let definition = registry.clone();
    host.serve(
        issue,
        Rc::new(move |_, _, declare| {
            declare.register_method(&definition, "issue", json!({}), "run")?;
            let marker = expected.clone();
            let calls = calls.clone();
            Ok(Implementation::new().method("run", move |value: i64, ctx| {
                assert_eq!(value, 5);
                assert_eq!(ctx.code_version(), 1);
                calls.set(calls.get() + 1);
                let marker = marker.clone();
                async move { Ok(marker) }.boxed_local()
            }))
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    let use_marker = token("use", Some(native), Some(portable.clone()));
    let definition = registry.clone();
    let expected = marker.clone();
    let calls = effects.clone();
    host.serve(
        use_marker,
        Rc::new(move |_, _, declare| {
            declare.register_method(&definition, "use", json!({}), "run")?;
            let expected = expected.clone();
            let calls = calls.clone();
            Ok(
                Implementation::new().method("run", move |value: Rc<Marker>, _| {
                    assert!(Rc::ptr_eq(&value, &expected));
                    calls.set(calls.get() + 1);
                    async { Ok(9_i64) }.boxed_local()
                }),
            )
        }),
        ServeOptions {
            lifetime: Lifetime::Host,
            ..Default::default()
        },
    )
    .unwrap();
    let consumer = token("consumer", None, Some(portable));
    let source = format!(
        r#"
const token = {};
export default {{
    name: 'consumer', uses: [], setup(scope, d) {{
        const view = scope.registry({{
            name: 'operations', conflict: 'error'
        }});
        let latest, seen = 0, old;
        const stop = view.subscribe(table => {{
            latest = table;
            view.table();
            seen++;
        }});
        d.serve(token, () => ({{
            methods: {{
                async run(ctx) {{
                    if (seen === 0 || latest.entries.length !== 2)
                        throw Error('registry observation');
                    if (old) {{
                        try {{
                            await latest.get('issue').operation.call(old, {{
                                present: true, value: 5
                            }});
                            throw Error('stale accepted');
                        }}
                        catch (e) {{
                            if (e.reason !== 'expired')
                                throw e;
                        }}
                    }}
                    try {{
                        await latest.get('use').operation.call(ctx, {{
                            present: true, value: {{}}
                        }});
                        throw Error('forgery accepted');
                    }}
                    catch (e) {{
                        if (e.reason !== 'native_type')
                            throw e;
                    }}
                    const issued = await latest.get('issue').operation.call(ctx, {{
                        present: true, value: 5
                    }});
                    if (!issued.present)
                        throw Error('missing capability');
                    const used = await latest.get('use').operation.call(ctx, issued);
                    if (!used.present)
                        throw Error('missing result');
                    old = ctx;
                    return used.value;
                }}
            }}, state: {{}}
        }}));
    }}
}};
"#,
        serde_json::to_string(consumer.descriptor()).unwrap()
    );
    let facet = executor
        .prepare_for_host(
            &host,
            BundledJsArtifact::new("registry", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();
    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(consumer, json!({}), GetOptions::default())
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            reference.call_json("run", None, None).await.unwrap(),
            Some(json!(9))
        );
    }

    assert_eq!(effects.get(), 4);
    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().contexts, 0);
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
}
