use futures::FutureExt;
use serde_json::json;
use std::{collections::BTreeMap, rc::Rc, time::Duration};
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

struct UnitCodec;
impl ScopedCodec<()> for UnitCodec {
    fn encode(&self, (): ()) -> Result<NativeValue> {
        Ok(NativeValue::absent())
    }

    fn decode(&self, value: NativeValue) -> Result<()> {
        if value.is_absent() {
            Ok(())
        } else {
            Err(RuntimeError::invalid("unit", "expected absence"))
        }
    }
}

fn run(test: impl FnOnce(Rc<TokioDriver>) -> futures::future::LocalBoxFuture<'static, ()>) {
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

fn token(name: &str, members: BTreeMap<String, Member>) -> Rc<Contract> {
    Rc::new(Contract::local(name, 1, json!({}), members).unwrap())
}

async fn install(host: &Host, executor: &Executor, source: String) {
    let facet = executor
        .prepare_for_host(
            host,
            BundledJsArtifact::new("capabilities", source, vec!["default".into()]).unwrap(),
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
fn opaque_resource_roundtrip_preserves_actual_native_rc_without_factory_value_read() {
    run(|driver| check_opaque_roundtrip(driver).boxed_local());
}

#[test]
fn captured_native_capability_cannot_cross_module_generation_or_host() {
    run(|driver| check_capability_provenance(driver).boxed_local());
}

#[test]
fn resource_signal_waiter_settles_on_ordinary_last_release_before_host_close() {
    run(|driver| check_resource_signal(driver).boxed_local());
}

async fn check_opaque_roundtrip(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("opaque", json!(null)), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    executor
        .register_native_opaque("marker@1", Rc::new(MarkerCodec))
        .unwrap();
    executor
        .register_native_method(
            NativeMethodSignature::new(None, Some("marker@1".into())).unwrap(),
            NativeMethodCodec::new(Rc::new(UnitCodec), Rc::new(MarkerCodec)),
        )
        .unwrap();
    let native = token(
        "native",
        BTreeMap::from([(
            "marker".into(),
            Member::Opaque {
                id: "marker@1".into(),
            },
        )]),
    );
    let exported = token(
        "exported",
        BTreeMap::from([(
            "marker".into(),
            Member::Opaque {
                id: "marker@1".into(),
            },
        )]),
    );
    let marker = Rc::new(Marker);
    let expected = marker.clone();
    host.serve(
        native.clone(),
        Rc::new(move |_, _, declare| {
            let marker = expected.clone();
            let resource = declare.resource(
                "marker",
                ResourceDeclaration::new(move |_| async { Ok(()) }.boxed_local()),
            )?;
            Ok(Implementation::new().map_opaque("marker", resource, move |_| Ok(marker.clone())))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source = format!(
        r#"
const native = {}, exported = {};
export default {{
    name: 'opaque', uses: [
        native
    ], setup(scope, d) {{
        d.serve(exported, (input, self) => {{
            const dependency = self.get(native, {{}});
            const resource = self.resource('marker', {{
                deps: [
                    dependency
                ], open: () => dependency.marker
            }});
            try {{
                resource.value;
                throw Error('factory read accepted');
            }}
            catch (e) {{
                if (e.reason !== 'factory')
                    throw e;
            }}
            return {{
                methods: {{}}, state: {{}}, opaque: {{
                    marker: resource
                }}
            }};
        }});
    }}
}};
"#,
        serde_json::to_string(native.descriptor()).unwrap(),
        serde_json::to_string(exported.descriptor()).unwrap()
    );
    install(&host, &executor, source).await;
    let reference = host
        .get(exported, json!({}), GetOptions::default())
        .await
        .unwrap();
    let value = reference.opaque::<Marker>("marker").unwrap();
    assert!(Rc::ptr_eq(&marker, &value));
    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
    assert_eq!(executor.stats().contexts, 0);
    assert_eq!(executor.stats().physical_jobs, 0);
}

async fn check_capability_provenance(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("first", json!(null)), driver.clone());
    let other = Host::new(HostOptions::new("second", json!(null)), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    executor
        .register_native_method(
            NativeMethodSignature::new(Some("marker@1".into()), Some("marker@1".into())).unwrap(),
            NativeMethodCodec::new(Rc::new(MarkerCodec), Rc::new(MarkerCodec)),
        )
        .unwrap();
    let method = Member::Method {
        args: Some(ValueSchema::Native("marker@1".into())),
        result: Some(ValueSchema::Native("marker@1".into())),
    };
    let first = token(
        "first",
        BTreeMap::from([
            ("run".into(), method.clone()),
            (
                "finish".into(),
                Member::Method {
                    args: None,
                    result: None,
                },
            ),
        ]),
    );
    let js = format!(
        r#"
const token = {};
export default {{
    name: 'first', uses: [], setup(s, d) {{
        d.serve(token, () => ({{
            methods: {{
                run: async (ctx, value) => {{
                    globalThis.captured = value;
                    await new Promise(resolve => globalThis.release = resolve);
                    return value;
                }}, finish: () => {{
                    globalThis.release();
                }}
            }}, state: {{}}
        }}));
    }}
}};
"#,
        serde_json::to_string(first.descriptor()).unwrap()
    );
    install(&host, &executor, js).await;
    let reference = host
        .get(first, json!({}), GetOptions::default())
        .await
        .unwrap();
    let marker = Rc::new(Marker);
    let mut call = reference
        .call::<Rc<Marker>, Rc<Marker>>("run", marker.clone(), None)
        .boxed_local();
    assert!(futures::poll!(&mut call).is_pending());
    while executor.stats().physical_jobs == 0 {
        tokio::task::yield_now().await;
    }

    tokio::task::yield_now().await;
    for (target, name) in [(&host, "next-generation"), (&other, "other-host")] {
        let contract = token(name, BTreeMap::from([("run".into(), method.clone())]));
        let source = format!(
            r#"
const token = {};
export default {{
    name: '{}', uses: [], setup(s, d) {{
        d.serve(token, () => ({{
            methods: {{
                run: () => globalThis.captured
            }}, state: {{}}
        }}));
    }}
}};
"#,
            serde_json::to_string(contract.descriptor()).unwrap(),
            name
        );
        install(target, &executor, source).await;
        let ref2 = target
            .get(contract, json!({}), GetOptions::default())
            .await
            .unwrap();
        assert_eq!(
            ref2.call::<Rc<Marker>, Rc<Marker>>("run", marker.clone(), None)
                .await
                .err()
                .unwrap()
                .reason,
            "foreign_generation"
        );
        ref2.release();
    }

    reference.call_json("finish", None, None).await.unwrap();
    let value = call.await.unwrap();
    assert!(Rc::ptr_eq(&marker, &value));
    reference.release();
    host.close().await.unwrap();
    other.close().await.unwrap();
    assert_eq!(executor.stats().contexts, 0);
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
}

async fn check_resource_signal(driver: Rc<TokioDriver>) {
    let host = Host::new(
        HostOptions::new("resource-signal", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    let empty = token("empty", BTreeMap::new());
    let source = format!(
        r#"
const token = {};
export default {{
    name: 'empty', uses: [], setup(s, d) {{
        d.serve(token, (i, self) => {{
            self.resource('r', {{
                open: signal => {{
                    void signal.cancelled;
                    return 1;
                }}
            }});
            return {{
                methods: {{}}, state: {{}}
            }};
        }});
    }}
}};
"#,
        serde_json::to_string(empty.descriptor()).unwrap()
    );
    install(&host, &executor, source).await;
    let reference = host
        .get(empty, json!({}), GetOptions::default())
        .await
        .unwrap();
    reference.release();
    while executor.stats().physical_jobs != 0 {
        tokio::task::yield_now().await;
    }

    assert_eq!(executor.stats().contexts, 0);
    host.close().await.unwrap();
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().capabilities, 0);
}

#[test]
fn kept_opaque_resource_remints_semantic_rc_without_accepting_old_native_tokens() {
    run(|driver| check_kept_opaque(driver).boxed_local());
}
async fn check_kept_opaque(driver: Rc<TokioDriver>) {
    let host = Host::new(HostOptions::new("kept-opaque", json!(null)), driver.clone());
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    executor
        .register_native_opaque("marker@1", Rc::new(MarkerCodec))
        .unwrap();
    executor
        .register_native_method(
            NativeMethodSignature::new(Some("marker@1".into()), Some("marker@1".into())).unwrap(),
            NativeMethodCodec::new(Rc::new(MarkerCodec), Rc::new(MarkerCodec)),
        )
        .unwrap();
    let native = token(
        "native-marker",
        BTreeMap::from([
            (
                "marker".into(),
                Member::Opaque {
                    id: "marker@1".into(),
                },
            ),
            (
                "echo".into(),
                Member::Method {
                    args: Some(ValueSchema::Native("marker@1".into())),
                    result: Some(ValueSchema::Native("marker@1".into())),
                },
            ),
        ]),
    );
    let exported = token(
        "kept-marker",
        BTreeMap::from([
            (
                "marker".into(),
                Member::Opaque {
                    id: "marker@1".into(),
                },
            ),
            (
                "old".into(),
                Member::Method {
                    args: None,
                    result: Some(ValueSchema::Portable(json!({"type":"boolean"}))),
                },
            ),
        ]),
    );
    let marker = Rc::new(Marker);
    let actual = marker.clone();
    let calls = Rc::new(std::cell::Cell::new(0));
    let effects = calls.clone();
    host.serve(
        native.clone(),
        Rc::new(move |_, _, declare| {
            let marker = actual.clone();
            let resource = declare.resource(
                "marker",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()),
            )?;
            let effects = effects.clone();
            Ok(Implementation::new()
                .map_opaque("marker", resource, move |_| Ok(marker.clone()))
                .method("echo", move |value: Rc<Marker>, _| {
                    effects.set(effects.get() + 1);
                    async move { Ok(value) }.boxed_local()
                }))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let prepare = || {
        executor.prepare_for_host(
            &host,
            BundledJsArtifact::new(
                "kept-opaque",
                format!(
                    r#"
const native = {}, exported = {};
export default {{name: 'kept-opaque', uses: [native], setup(scope, declare) {{
    declare.serve(exported, (input, self) => {{
        const dependency = self.get(native, {{}});
        const resource = self.resource('kept', {{
            deps: [dependency], open: () => dependency.marker
        }});
        return {{methods: {{async old(ctx) {{
            try {{await dependency.echo(resource.value, ctx);}}
            catch (error) {{
                if (error.reason !== 'foreign_generation') throw error;
                return true;
            }}
            return false;
        }}}}, state: {{}}, opaque: {{marker: resource}}}};
    }}, {{lifetime: 'host'}});
}}}};
"#,
                    serde_json::to_string(native.descriptor()).unwrap(),
                    serde_json::to_string(exported.descriptor()).unwrap()
                ),
                vec!["default".into()],
            )
            .unwrap(),
            "default".into(),
        )
    };
    host.install_executable(prepare().await.unwrap(), LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
    let reference = host
        .get(exported.clone(), json!({}), GetOptions::default())
        .await
        .unwrap();
    assert!(Rc::ptr_eq(
        &marker,
        &reference.opaque::<Marker>("marker").unwrap()
    ));
    host.reload(vec![prepare().await.unwrap()], ReloadOptions::default())
        .await
        .unwrap();
    assert!(Rc::ptr_eq(
        &marker,
        &reference.opaque::<Marker>("marker").unwrap()
    ));
    assert_eq!(
        reference.call_json("old", None, None).await.unwrap(),
        Some(json!(true))
    );
    assert_eq!(calls.get(), 0);
    reference.release();
    host.close().await.unwrap();
    assert_eq!(executor.stats().callbacks, 0);
    assert_eq!(executor.stats().physical_jobs, 0);
    assert_eq!(executor.stats().capabilities, 0);
}
