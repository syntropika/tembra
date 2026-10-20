use futures::FutureExt;
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, rc::Rc, time::Duration};
use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};
use tembra_executor_js::*;
use tembra_native::TokioDriver;
fn method(args: bool) -> Member {
    Member::Method {
        args: args.then(|| ValueSchema::Portable(json!({}))),
        result: Some(ValueSchema::Portable(json!({}))),
    }
}

fn token(name: &str, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

#[test]
fn foreign_portable_values_refuse_loss_without_getters_or_native_effects_and_preserve_depth_edge() {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(
                Duration::from_secs(15),
                check_portable_values(driver.clone()),
            )
            .await
            .unwrap();
        }));
}

#[test]
fn initial_derived_and_draft_state_ingress_validate_before_reading_properties() {
    let driver = TokioDriver::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(driver.run_until(async {
            tokio::time::timeout(Duration::from_secs(15), check_state_values(driver.clone()))
                .await
                .unwrap();
        }));
}

async fn check_portable_values(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("portable", json!(null)), driver.clone());
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let echo = token("echo", BTreeMap::from([("run".into(), method(true))]));
    let effects = Rc::new(Cell::new(0));
    let counter = effects.clone();
    host.serve(
        echo.clone(),
        Rc::new(move |_, _, _| {
            let counter = counter.clone();
            Ok(Implementation::new().json_method("run", move |args, _| {
                counter.set(counter.get() + 1);
                async move { Ok(args) }.boxed_local()
            }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let consumer = token(
        "consumer",
        BTreeMap::from([
            ("bad".into(), method(true)),
            ("argument".into(), method(true)),
            ("getters".into(), method(false)),
            ("edge".into(), method(true)),
            ("intrinsics".into(), method(true)),
        ]),
    );
    let source = format!(
        r#"
const echo = {}, consumer = {};
let getters = 0;
const invalid = index => {{
    switch (index) {{
        case 0: return {{
            [Symbol('secret')]: 1
        }};
        case 1: return Object.defineProperty({{}}, 'hidden', {{
            value: 1
        }});
        case 2: return {{
            get value() {{
                getters++;
                return 1;
            }}
        }};
        case 3: return Array(2);
        case 4: {{
            const value = [
                1
            ];
            value.custom = 2;
            return value;
        }}
        case 5: return new (class extends Array {{
        }})(1);
        case 6: return new Date(0);
        case 7: return NaN;
        case 8: return Number.MAX_SAFE_INTEGER + 1;
        case 9: return '\ud800';
        case 10: return '\udfff';
        case 11: return {{
            ['\ud800']: 1
        }};
        case 12: {{
            const value = [
                1
            ];
            Object.defineProperty(value, '0', {{
                get() {{
                    getters++;
                    return 1;
                }}, enumerable: true
            }});
            return value;
        }}
        case 13: {{
            const value = {{}};
            value.self = value;
            return value;
        }}
        case 14: return undefined;
        case 15: return Infinity;
        case 16: return Object.setPrototypeOf({{
            value: 1
        }}, {{
            custom: true
        }});
        case 17: {{
            let value = 0;
            for (let n = 0; n < 129; n++)
                value = [
                    value
                ];
            return value;
        }}
    }}
}};
export default {{
    name: 'portable', uses: [
        echo
    ], setup(scope, d) {{
        d.serve(consumer, (input, self) => {{
            const dependency = self.get(echo, {{}});
            return {{
                methods: {{
                    bad: (ctx, index) => invalid(index),
                    async argument(ctx, index) {{
                        try {{
                            await dependency.run(invalid(index), ctx);
                            return false;
                        }}
                        catch (e) {{
                            return true;
                        }}
                    }},
                    getters: () => getters,
                    intrinsics(ctx, mode) {{
                        const value = mode === 0 ? {{nested: {{answer: 42}}}} : invalid(mode - 1);
                        Number.isFinite = () => true;
                        Number.isInteger = () => false;
                        Number.isSafeInteger = () => true;
                        Object.getPrototypeOf = () => null;
                        Object.getOwnPropertyDescriptor = () => ({{value: 1, enumerable: true}});
                        Object.hasOwn = () => true;
                        Reflect.ownKeys = () => [];
                        Array.isArray = () => false;
                        Set = class {{}};
                        String.prototype.charCodeAt = () => 65;
                        if (mode === 0 && !value.nested.hasOwnProperty('answer'))
                            throw Error('portable object lost its ordinary prototype');
                        return value;
                    }},
                    async edge(ctx, depth) {{
                        let value = 0;
                        for (let n = 0; n < depth; n++)
                            value = [
                                value
                            ];
                        return await dependency.run(value, ctx);
                    }}
                }}, state: {{}}
            }};
        }});
    }}
}};
"#,
        serde_json::to_string(echo.descriptor()).unwrap(),
        serde_json::to_string(consumer.descriptor()).unwrap()
    );
    let artifact = BundledJsArtifact::new("portable", source, vec!["default".into()]).unwrap();
    let facet = executor
        .prepare(host.id(), artifact, "default".into())
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
    for index in 0..18 {
        assert!(
            reference
                .call_json("bad", Some(json!(index)), None)
                .await
                .is_err(),
            "invalid result {index}"
        );
        assert_eq!(
            reference
                .call_json("argument", Some(json!(index)), None)
                .await
                .unwrap(),
            Some(json!(true)),
            "invalid args {index}"
        );
    }

    assert_eq!(effects.get(), 0);
    assert_eq!(
        reference.call_json("getters", None, None).await.unwrap(),
        Some(json!(0))
    );
    let mut result = reference
        .call_json("edge", Some(json!(128)), None)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..128 {
        result = result.as_array().unwrap()[0].clone();
    }
    assert_eq!(result, json!(0));
    assert_eq!(effects.get(), 1);
    assert_eq!(
        reference
            .call_json("intrinsics", Some(json!(0)), None)
            .await
            .unwrap(),
        Some(json!({"nested":{"answer":42}}))
    );
    for index in [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
    ] {
        assert!(
            reference
                .call_json("intrinsics", Some(json!(index)), None)
                .await
                .is_err(),
            "patched intrinsic accepted invalid portable value {index}"
        );
    }
    assert_eq!(
        reference.call_json("getters", None, None).await.unwrap(),
        Some(json!(0))
    );
    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().contexts, 0);
    assert_eq!(executor.stats().physical_jobs, 0);
}

async fn check_state_values(driver: Rc<TokioDriver>) {
    let host = Host::new(
        HostOptions::new("state-profile", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();
    let contract = token(
        "state-profile",
        BTreeMap::from([("run".into(), method(true))]),
    );
    let source = format!(
        r#"
const token = {};
let getters = 0;
const bad = () => ({{
    get value() {{
        getters++;
        return 9;
    }}
}});
export default {{
    name: 'state-profile', uses: [], setup(scope, d) {{
        d.serve(token, (input, self) => {{
            try {{
                self.cell('invalid', {{
                    schema: {{}}, initial: bad()
                }});
                throw Error('invalid initial accepted');
            }}
            catch (e) {{
                if (e.message === 'invalid initial accepted')
                    throw e;
            }}
            const count = self.cell('count', {{
                schema: {{
                    type: 'integer'
                }}, initial: 0
            }});
            const projected = self.derive([
                count
            ], {{}}, ([snapshot]) => snapshot.value === 0 ? 0 : bad());
            return {{
                methods: {{
                    run(ctx, mode) {{
                        try {{
                            if (mode === 0)
                                count.replace(ctx, 1);
                            else
                                count.change(ctx, draft => {{
                                    Object.defineProperty(draft, 'value', {{
                                        get() {{
                                            getters++;
                                            return 1;
                                        }}, enumerable: true
                                    }});
                                }});
                            throw Error('invalid update accepted');
                        }}
                        catch (e) {{
                            if (e.message === 'invalid update accepted')
                                throw e;
                        }}
                        return {{
                            count: count.value, projected: projected.value, getters
                        }};
                    }}
                }}, state: {{}}
            }};
        }});
    }}
}};
"#,
        serde_json::to_string(contract.descriptor()).unwrap()
    );
    let artifact = BundledJsArtifact::new("state-profile", source, vec!["default".into()]).unwrap();
    let facet = executor
        .prepare(host.id(), artifact, "default".into())
        .await
        .unwrap();
    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(contract, json!({}), GetOptions::default())
        .await
        .unwrap();
    for mode in 0..2 {
        assert_eq!(
            reference
                .call_json("run", Some(json!(mode)), None)
                .await
                .unwrap(),
            Some(json!({
            "count":0,"projected":0,"getters":0}
            ))
        );
    }

    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().callbacks, 0);
}
