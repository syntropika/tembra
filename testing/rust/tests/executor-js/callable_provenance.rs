use futures::{
    FutureExt,
    future::{Either, LocalBoxFuture, select},
};

use serde_json::json;

use std::{cell::Cell, collections::BTreeMap, rc::Rc, time::Duration};

use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

use tembra_executor_js::*;

use tembra_native::TokioDriver;

struct Identity;

impl ScopedCodec<NativeValue> for Identity {
    fn encode(&self, value: NativeValue) -> Result<NativeValue> {
        Ok(value)
    }

    fn decode(&self, value: NativeValue) -> Result<NativeValue> {
        Ok(value)
    }
}

fn token(name: &str, release: bool) -> Rc<Contract> {
    let mut members = BTreeMap::from([(
        "run".into(),
        Member::Method {
            args: Some(ValueSchema::Native("views@1".into())),
            result: Some(ValueSchema::Native("views@1".into())),
        },
    )]);

    if release {
        members.insert(
            "finish".into(),
            Member::Method {
                args: None,
                result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
            },
        );
    }

    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

async fn install(host: &Host, executor: &Executor, name: &str, source: String) {
    let facet = executor
        .prepare(
            host.id(),
            BundledJsArtifact::new(name, source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();

    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
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

#[test]
fn decoded_callables_reuse_original_authority_and_never_rebind_to_later_receivers() {
    run(|driver| {
        async move {
            let a = Host::new(HostOptions::new("a", json!("a")), driver.clone());
            let b = Host::new(HostOptions::new("b", json!("b")), driver.clone());
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();

            executor
                .register_native_method(
                    NativeMethodSignature::new(Some("views@1".into()), Some("views@1".into()))
                        .unwrap(),
                    NativeMethodCodec::new(Rc::new(Identity), Rc::new(Identity)),
                )
                .unwrap();

            let native = token("native", false);
            a.serve(
                native.clone(),
                Rc::new(|_, _, _| {
                    Ok(
                        Implementation::new().method("run", |input: NativeValue, _| {
                            async move { Ok(input) }.boxed_local()
                        }),
                    )
                }),
                ServeOptions::default(),
            )
            .unwrap();

            let source = token("source", true);
            let same_host = token("same-host", false);
            let other_host = token("other-host", false);

            install(
                &a,
                &executor,
                "source",
                format!(
                    r#"
const source = {}, native = {};
export default {{
    name: 'source', uses: [], setup(scope, d) {{
        d.serve(source, (input, self) => {{
            const dependency = self.get(native, {{}});
            return {{
                methods: {{
                    async run(ctx, args) {{
                        globalThis.saved = args.action;
                        const issued = await dependency.run(args.action, ctx);
                        const value = await issued(7);
                        if (value !== 7)
                            throw Error('roundtrip callable failed');
                        await new Promise(resolve => globalThis.release = resolve);
                        return {{
                            nested: {{
                                callback: issued
                            }}
                        }};
                    }}, finish() {{
                        globalThis.release();
                        return 1;
                    }}
                }}, state: {{}}
            }};
        }});
    }}
}};
"#,
                    serde_json::to_string(source.descriptor()).unwrap(),
                    serde_json::to_string(native.descriptor()).unwrap()
                ),
            )
            .await;

            for (host, token) in [(&a, same_host.clone()), (&b, other_host.clone())] {
                install(
                    host,
                    &executor,
                    "reader",
                    format!(
                        r#"
const token = {};
export default {{
    name: 'reader', uses: [], setup(scope, d) {{
        d.serve(token, () => ({{
            methods: {{
                run: () => globalThis.saved
            }}, state: {{}}
        }}));
    }}
}};
"#,
                        serde_json::to_string(token.descriptor()).unwrap()
                    ),
                )
                .await;
            }

            let effects = Rc::new(Cell::new(0));
            let calls = effects.clone();
            let callback = NativeCallable::new(Rc::new(move |input| {
                calls.set(calls.get() + 1);
                async move { Ok(input) }.boxed_local()
            }));

            let input = NativeValue::record(
                NativeRecord::new([("action".into(), NativeValue::callable(callback))]).unwrap(),
            );

            let held = a
                .get(source, json!({}), GetOptions::default())
                .await
                .unwrap();
            let pending = held
                .call::<NativeValue, NativeValue>("run", input, None)
                .boxed_local();

            let Either::Right(((), pending)) = select(
                pending,
                tokio::time::sleep(Duration::from_millis(10)).boxed_local(),
            )
            .await
            else {
                panic!("source must remain active");
            };
            assert_eq!(effects.get(), 1);

            for (host, token) in [(&a, same_host), (&b, other_host)] {
                let foreign = host
                    .get(token, json!({}), GetOptions::default())
                    .await
                    .unwrap();
                let error = foreign
                    .call::<NativeValue, NativeValue>("run", NativeValue::portable(json!(0)), None)
                    .await
                    .err()
                    .unwrap();
                assert_eq!(error.reason, "foreign_generation");
                foreign.release();
            }

            assert_eq!(
                held.call_json("finish", None, None).await.unwrap(),
                Some(json!(1))
            );

            let returned = pending.await.unwrap().into_record().unwrap();
            let nested = returned
                .get("nested")
                .unwrap()
                .clone()
                .into_record()
                .unwrap();
            let callback = nested
                .get("callback")
                .unwrap()
                .clone()
                .into_callable()
                .unwrap();

            let input = NativeValue::record(
                NativeRecord::new([("action".into(), NativeValue::callable(callback))]).unwrap(),
            );

            assert_eq!(
                held.call::<NativeValue, NativeValue>("run", input, None)
                    .await
                    .err()
                    .unwrap()
                    .reason,
                "expired"
            );
            assert_eq!(effects.get(), 1);

            held.release();
            b.close().await.unwrap();
            a.close().await.unwrap();
            assert_eq!(executor.stats().physical_jobs, 0);
            assert_eq!(executor.stats().contexts, 0);
            assert_eq!(executor.stats().callbacks, 0);
            assert_eq!(executor.stats().capabilities, 0);
        }
        .boxed_local()
    });
}

#[test]
fn deferred_raw_input_observes_mutation_before_driver_poll() {
    run(|driver| {
        async move {
            let host = Host::new(HostOptions::new("raw-view", json!(null)), driver.clone());
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            executor
                .register_native_method(
                    NativeMethodSignature::new(Some("views@1".into()), Some("views@1".into()))
                        .unwrap(),
                    NativeMethodCodec::new(Rc::new(Identity), Rc::new(Identity)),
                )
                .unwrap();
            let token = token("raw-view", false);
            install(
                &host,
                &executor,
                "raw-view",
                format!(
                    r#"
const token = {};
export default {{name: 'raw-view', uses: [], setup(scope, declare) {{
    declare.serve(token, () => ({{methods: {{async run(ctx, args) {{
        const input = {{n: 4}};
        const pending = args.action(input);
        input.n = 5;
        return (await pending).n * 2;
    }}}}, state: {{}}}}));
}}}};
"#,
                    serde_json::to_string(token.descriptor()).unwrap()
                ),
            )
            .await;
            let reference = host
                .get(token, json!({}), GetOptions::default())
                .await
                .unwrap();
            let callback = NativeCallable::new(Rc::new(|value| async { Ok(value) }.boxed_local()));
            let input = NativeValue::record(
                NativeRecord::new([("action".into(), NativeValue::callable(callback))]).unwrap(),
            );
            let output: NativeValue = reference.call("run", input, None).await.unwrap();
            assert_eq!(output.into_portable().unwrap(), json!(10));
            reference.release();
            host.close().await.unwrap();
            assert_eq!(executor.stats().physical_jobs, 0);
            assert_eq!(executor.stats().callbacks, 0);
        }
        .boxed_local()
    });
}

#[test]
fn portable_native_position_keeps_ordinary_objects_after_application_patches_intrinsics() {
    run(|driver| {
        async move {
            let host = Host::new(
                HostOptions::new("portable-position", json!(null)),
                driver.clone(),
            );
            let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
            let argument = ValueSchema::Portable(json!({
                "type":"object", "properties":{"value":{"type":"integer"}},
                "required":["value"], "additionalProperties":false
            }));
            let result = ValueSchema::Native("views@1".into());
            executor
                .register_native_method(
                    NativeMethodSignature::from_schemas(
                        Some(argument.clone()),
                        Some(result.clone()),
                    )
                    .unwrap(),
                    NativeMethodCodec::new(Rc::new(Identity), Rc::new(Identity)),
                )
                .unwrap();
            let token = Rc::new(
                Contract::local(
                    "portable-position",
                    1,
                    json!({}),
                    BTreeMap::from([(
                        "run".into(),
                        Member::Method {
                            args: Some(argument),
                            result: Some(result),
                        },
                    )]),
                )
                .unwrap(),
            );
            install(
                &host,
                &executor,
                "portable-position",
                format!(
                    r#"
const token = {};
export default {{name: 'portable-position', uses: [], setup(scope, declare) {{
    declare.serve(token, () => ({{methods: {{run(ctx, args) {{
        if (!args.hasOwnProperty('value') || `${{args}}` !== '[object Object]')
            throw Error('portable object behavior changed');
        Object.getOwnPropertyDescriptor = () => {{throw Error('patched descriptor');}};
        Object.getPrototypeOf = () => null;
        Reflect.ownKeys = () => [];
        Object.defineProperty = () => {{throw Error('patched defineProperty');}};
        Number.isFinite = () => false;
        Set = class {{}};
        return {{value: args.value}};
    }}}}, state: {{}}}}));
}}}};
"#,
                    serde_json::to_string(token.descriptor()).unwrap()
                ),
            )
            .await;
            let reference = host
                .get(token, json!({}), GetOptions::default())
                .await
                .unwrap();
            for value in [4, 5] {
                let output: NativeValue = reference
                    .call("run", NativeValue::portable(json!({"value":value})), None)
                    .await
                    .unwrap();
                assert_eq!(output.into_portable().unwrap(), json!({"value":value}));
            }
            reference.release();
            host.close().await.unwrap();
            assert_eq!(executor.stats().physical_jobs, 0);
            assert_eq!(executor.stats().callbacks, 0);
        }
        .boxed_local()
    });
}

#[test]
fn native_record_fields_survive_replaced_array_iterator_and_push() {
    run(|driver| check_record_intrinsics(driver).boxed_local());
}
async fn check_record_intrinsics(driver: Rc<TokioDriver>) {
    let host = Host::new(
        HostOptions::new("record-intrinsics", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    executor
        .register_native_method(
            NativeMethodSignature::new(Some("views@1".into()), Some("views@1".into())).unwrap(),
            NativeMethodCodec::new(Rc::new(Identity), Rc::new(Identity)),
        )
        .unwrap();
    let token = token("record-intrinsics", false);
    install(
        &host,
        &executor,
        "record-intrinsics",
        format!(
            r#"
const token = {};
export default {{name: 'record-intrinsics', uses: [], setup(scope, declare) {{
    declare.serve(token, () => ({{methods: {{run(ctx, args) {{
        Array.prototype.push = () => 0;
        Array.prototype[Symbol.iterator] = function* () {{}};
        return {{nested: {{cap: args.cap, count: args.count}}, optional: 'retained'}};
    }}}}, state: {{}}}}));
}}}};
"#,
            serde_json::to_string(token.descriptor()).unwrap()
        ),
    )
    .await;
    let reference = host
        .get(token, json!({}), GetOptions::default())
        .await
        .unwrap();
    let marker = Rc::new(String::from("actual native marker"));
    let input = NativeValue::record(
        NativeRecord::new([
            ("cap".into(), NativeValue::capability(marker.clone())),
            ("count".into(), NativeValue::portable(json!(4))),
        ])
        .unwrap(),
    );
    let output: NativeValue = reference.call("run", input, None).await.unwrap();
    let output = output.into_record().unwrap();
    assert_eq!(output.entries().len(), 2);
    let nested = output.get("nested").unwrap().clone().into_record().unwrap();
    assert_eq!(nested.entries().len(), 2);
    assert!(Rc::ptr_eq(
        &marker,
        &nested
            .get("cap")
            .unwrap()
            .clone()
            .downcast::<String>()
            .unwrap()
    ));
    assert_eq!(
        nested
            .get("count")
            .unwrap()
            .clone()
            .into_portable()
            .unwrap(),
        json!(4)
    );
    assert_eq!(
        output
            .get("optional")
            .unwrap()
            .clone()
            .into_portable()
            .unwrap(),
        json!("retained")
    );
    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().capabilities, 0);
    assert_eq!(executor.stats().physical_jobs, 0);
}
