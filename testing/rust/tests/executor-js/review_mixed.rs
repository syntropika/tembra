use futures::{FutureExt, future::LocalBoxFuture};

use serde_json::json;

use std::{cell::Cell, collections::BTreeMap, rc::Rc, time::Duration};

use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

use tembra_executor_js::*;

use tembra_native::TokioDriver;

#[derive(Debug)]
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

struct Integer;

impl ScopedCodec<i64> for Integer {
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

struct Permissive(Rc<Marker>);

impl ScopedCodec<Rc<Marker>> for Permissive {
    fn encode(&self, _: Rc<Marker>) -> Result<NativeValue> {
        Ok(NativeValue::absent())
    }

    fn decode(&self, _: NativeValue) -> Result<Rc<Marker>> {
        Ok(self.0.clone())
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

fn run(test: impl FnOnce(Rc<TokioDriver>) -> LocalBoxFuture<'static, ()>) {
    let driver = TokioDriver::new();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(15), test(driver.clone()))
                .await
                .unwrap();
        }));
}

async fn install(host: &Host, executor: &Executor, source: String) {
    let facet = executor
        .prepare_for_host(
            host,
            BundledJsArtifact::new("mixed", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();

    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
}

#[test]
fn mixed_foreign_arguments_validate_both_directions_and_repeated_results_stay_call_scoped() {
    run(|driver| mixed_arguments(driver).boxed_local());
}

#[test]
fn present_native_positions_refuse_absence_before_permissive_codecs_or_handlers() {
    run(|driver| {
        scenario_present_native_positions_refuse_absence_before_permissive_codecs_or_handlers(
            driver,
        )
        .boxed_local()
    });
}

struct PanickingInteger {
    encode: Rc<Cell<bool>>,
    decode: Rc<Cell<bool>>,
}

impl ScopedCodec<i64> for PanickingInteger {
    fn encode(&self, value: i64) -> Result<NativeValue> {
        assert!(!self.encode.replace(false), "encode probe");

        Integer.encode(value)
    }

    fn decode(&self, value: NativeValue) -> Result<i64> {
        assert!(!self.decode.replace(false), "decode probe");

        Integer.decode(value)
    }
}

#[test]
fn registered_codec_panics_are_typed_in_both_directions_and_engine_remains_usable() {
    run(|driver| {
        scenario_registered_codec_panics_are_typed_in_both_directions_and_engine_remains_usable(
            driver,
        )
        .boxed_local()
    });
}

struct Unit;
impl ScopedCodec<()> for Unit {
    fn encode(&self, (): ()) -> Result<NativeValue> {
        panic!("absent arguments must bypass codecs")
    }
    fn decode(&self, _: NativeValue) -> Result<()> {
        panic!("absent arguments must bypass codecs")
    }
}

#[test]
fn present_native_results_refuse_absence_before_a_permissive_result_decoder() {
    run(|driver| {
        scenario_present_native_results_refuse_absence_before_a_permissive_result_decoder(driver)
            .boxed_local()
    });
}

async fn mixed_arguments(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("mixed", json!("host")), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();

    let portable = ValueSchema::Portable(json!({
    "type":"integer","minimum":0}
    ));
    let native = ValueSchema::Native("marker@1".into());

    executor
        .register_native_method(
            NativeMethodSignature::from_schemas(Some(portable.clone()), Some(native.clone()))
                .unwrap(),
            NativeMethodCodec::new(Rc::new(Integer), Rc::new(MarkerCodec)),
        )
        .unwrap();

    let issue = token("issue", Some(portable.clone()), Some(native.clone()));
    let foreign = token("foreign", Some(portable.clone()), Some(native));
    let consumer = token("consumer", None, Some(portable));

    let registry = Registry::new("operations", ConflictPolicy::Error).unwrap();
    let registered = registry.clone();
    let effects = Rc::new(Cell::new(0));
    let calls = effects.clone();
    let marker = Rc::new(Marker);

    host.serve(
        issue.clone(),
        Rc::new(move |_, _, declare| {
            declare.register_method(&registered, "issue", json!({}), "run")?;
            let effects = calls.clone();
            let marker = marker.clone();

            Ok(Implementation::new().method("run", move |value: i64, _| {
                assert!(value >= 0);
                effects.set(effects.get() + 1);
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

    let source = format!(
        r#"
const issue={},foreign={},consumer={};
let effects=0;
export default {{
    name:'mixed',uses:[],setup(scope,d){{
        const view=scope.registry({{
            name:'operations',conflict:'error'
        }}
        );
        let latest;
        const stop=view.subscribe(table=>{{
            latest=table;
        }}
        );
        d.serve(foreign,()=>({{
            methods:{{
                run(ctx,value){{
                    effects++;
                    throw Error('invalid Rust argument reached JS');
                }}
            }}
            ,state:{{
            }}
        }}
        ));
        d.serve(consumer,(input,self)=>{{
            const dependency=self.get(issue,{{
            }}
            );
            return {{
                methods:{{
                    async run(ctx){{
                        try{{
                            await dependency.run(5);
                            throw Error('missing Context accepted');
                        }}
                        catch(e){{
                            if(e.reason!=='native_context')throw e;
                        }}
                        try{{
                            await dependency.run(-5,ctx);
                            throw Error('invalid argument accepted');
                        }}
                        catch(e){{
                            if(e.reason!=='validation')throw e;
                        }}
                        for(let i=0;
                        i<1500;
                        i++){{
                            if(scope.registry({{
                                name:'operations',conflict:'error'
                            }}
                            )!==view)throw Error('unstable view');
                            view.table(ctx);
                            await dependency.run(5,ctx);
                        }}
                        if(!latest)throw Error('missing initial table');
                        return effects;
                    }}
                }}
                ,state:{{
                }}
            }}
            ;
        }}
        );
    }}
}}
;
"#,
        serde_json::to_string(issue.descriptor()).unwrap(),
        serde_json::to_string(foreign.descriptor()).unwrap(),
        serde_json::to_string(consumer.descriptor()).unwrap()
    );

    install(&host, &executor, source).await;

    let reference = host
        .get(foreign, json!({}), GetOptions::default())
        .await
        .unwrap();

    assert_eq!(
        reference
            .call::<i64, Rc<Marker>>("run", -5, None)
            .await
            .unwrap_err()
            .reason,
        "validation"
    );
    reference.release();

    let reference = host
        .get(consumer, json!({}), GetOptions::default())
        .await
        .unwrap();

    for _ in 0..2 {
        assert_eq!(
            reference.call_json("run", None, None).await.unwrap(),
            Some(json!(0))
        );
        assert!(executor.stats().capabilities < 20);
        assert_eq!(executor.stats().contexts, 0);
    }

    assert_eq!(effects.get(), 3000);
    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().capabilities, 0);
}

async fn scenario_present_native_positions_refuse_absence_before_permissive_codecs_or_handlers(
    driver: Rc<TokioDriver>,
) {
    let host = Host::new(HostOptions::new("presence", json!("host")), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let marker = Rc::new(Marker);
    let native = ValueSchema::Native("marker@1".into());

    executor
        .register_native_method(
            NativeMethodSignature::new(Some("marker@1".into()), Some("marker@1".into())).unwrap(),
            NativeMethodCodec::new(
                Rc::new(Permissive(marker.clone())),
                Rc::new(Permissive(marker.clone())),
            ),
        )
        .unwrap();

    let foreign = token("foreign", Some(native.clone()), Some(native.clone()));
    let consumer = token(
        "consumer",
        None,
        Some(ValueSchema::Portable(json!({
        "type":"integer"}
        ))),
    );

    let source = format!(
        r#"
const foreign={},consumer={};
let effects=0;
export default {{
    name:'presence',uses:[],setup(scope,d){{
        d.serve(foreign,()=>({{
            methods:{{
                run(ctx,value){{
                    effects++;
                    return undefined;
                }}
            }}
            ,state:{{
            }}
        }}
        ));
        d.serve(consumer,(input,self)=>{{
            const dependency=self.get(foreign,{{
            }}
            );
            return {{
                methods:{{
                    async run(ctx){{
                        try{{
                            await dependency.run(undefined,ctx);
                            throw Error('missing accepted');
                        }}
                        catch(e){{
                            if(e.reason!=='native_presence')throw e;
                        }}
                        return effects;
                    }}
                }}
                ,state:{{
                }}
            }}
            ;
        }}
        );
    }}
}}
;
"#,
        serde_json::to_string(foreign.descriptor()).unwrap(),
        serde_json::to_string(consumer.descriptor()).unwrap()
    );

    install(&host, &executor, source).await;
    let held = host
        .get(foreign, json!({}), GetOptions::default())
        .await
        .unwrap();

    assert_eq!(
        held.call::<Rc<Marker>, Rc<Marker>>("run", marker, None)
            .await
            .unwrap_err()
            .reason,
        "native_presence"
    );
    held.release();

    let held = host
        .get(consumer, json!({}), GetOptions::default())
        .await
        .unwrap();
    assert_eq!(
        held.call_json("run", None, None).await.unwrap(),
        Some(json!(0))
    );
    held.release();
    host.close().await.unwrap();
}

async fn scenario_registered_codec_panics_are_typed_in_both_directions_and_engine_remains_usable(
    driver: Rc<TokioDriver>,
) {
    let host = Host::new(
        HostOptions::new("panic-codec", json!("host")),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();

    let portable = ValueSchema::Portable(json!({
    "type":"integer"}
    ));
    let native = ValueSchema::Native("marker@1".into());

    executor
        .register_native_method(
            NativeMethodSignature::from_schemas(Some(portable.clone()), Some(native.clone()))
                .unwrap(),
            NativeMethodCodec::new(
                Rc::new(PanickingInteger {
                    encode: Rc::new(Cell::new(true)),
                    decode: Rc::new(Cell::new(true)),
                }),
                Rc::new(MarkerCodec),
            ),
        )
        .unwrap();

    let dependency = token("dependency", Some(portable.clone()), Some(native.clone()));
    let foreign = token("foreign", Some(portable), Some(native));
    let marker = Rc::new(Marker);
    let expected = marker.clone();
    let calls = Rc::new(Cell::new(0));
    let effects = calls.clone();

    host.serve(
        dependency.clone(),
        Rc::new(move |_, _, _| {
            let marker = marker.clone();
            let effects = effects.clone();
            Ok(Implementation::new().method("run", move |_: i64, _| {
                effects.set(effects.get() + 1);
                let marker = marker.clone();
                async move { Ok(marker) }.boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();

    let source = format!(
        r#"
const dependency={},foreign={};
export default {{
    name:'panic-codec',uses:[],setup(scope,d){{
        d.serve(foreign,(input,self)=>{{
            const held=self.get(dependency,{{
            }}
            );
            return {{
                methods:{{
                    run:(ctx,arg)=>held.run(arg,ctx)
                }}
                ,state:{{
                }}
            }}
            ;
        }}
        );
    }}
}}
;
"#,
        serde_json::to_string(dependency.descriptor()).unwrap(),
        serde_json::to_string(foreign.descriptor()).unwrap()
    );
    install(&host, &executor, source).await;

    let held = host
        .get(foreign, json!({}), GetOptions::default())
        .await
        .unwrap();

    for _ in 0..2 {
        let error = held
            .call::<i64, Rc<Marker>>("run", 5, None)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Invalid);
        assert_eq!(error.reason, "codec_panic");
        assert_eq!(calls.get(), 0);
    }

    let value = held.call::<i64, Rc<Marker>>("run", 5, None).await.unwrap();
    assert!(Rc::ptr_eq(&value, &expected));
    assert_eq!(calls.get(), 1);
    held.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().physical_jobs, 0);
}

async fn scenario_present_native_results_refuse_absence_before_a_permissive_result_decoder(
    driver: Rc<TokioDriver>,
) {
    let host = Host::new(HostOptions::new("result", json!("host")), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let native = ValueSchema::Native("present@1".into());
    executor
        .register_native_method(
            NativeMethodSignature::new(None, Some("present@1".into())).unwrap(),
            NativeMethodCodec::new(Rc::new(Unit), Rc::new(Permissive(Rc::new(Marker)))),
        )
        .unwrap();
    let foreign = token("foreign", None, Some(native));
    let source = format!(
        r#"
const token={};
export default {{
    name:'result',uses:[],setup(scope,d){{
        d.serve(token,()=>({{
            methods:{{
                run:()=>undefined
            }}
            ,state:{{
            }}
        }}
        ));
    }}
}}
;
"#,
        serde_json::to_string(foreign.descriptor()).unwrap()
    );
    install(&host, &executor, source).await;
    let reference = host
        .get(foreign, json!({}), GetOptions::default())
        .await
        .unwrap();
    let error = reference
        .call::<(), Rc<Marker>>("run", (), None)
        .await
        .unwrap_err();
    assert_eq!(error.reason, "native_presence");
    reference.release();
    host.close().await.unwrap();
}
