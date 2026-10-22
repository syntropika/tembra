use futures::{FutureExt, future::LocalBoxFuture};

use serde_json::json;

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};

use tembra_core::{
    contracts::{Contract, Member, ValueSchema},
    *,
};

use tembra_executor_js::*;

use tembra_hooks::*;

use tembra_native::TokioDriver;

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

async fn environment(driver: Rc<TokioDriver>) -> (Host, Executor, HookManager) {
    let host = Host::new(HostOptions::new("profiles", json!("host")), driver.clone());

    let executor = Executor::new(driver.clone(), ExecutorOptions::default()).unwrap();

    let scope = Rc::new(RefCell::new(None));

    let captured = scope.clone();

    host.install(
        NativeFacet::new("scope", vec![], move |scope, _| {
            *captured.borrow_mut() = Some(scope.clone());

            Ok(())
        })
        .unwrap(),
        LoadOptions::default(),
    )
    .unwrap()
    .await
    .unwrap();

    let manager = HookManager::new(
        scope.borrow().clone().unwrap(),
        driver,
        HookManagerOptions::default(),
    )
    .unwrap();

    (host, executor, manager)
}

async fn contribution(host: &Host, executor: &Executor, descriptor: &HookDescriptor, body: &str) {
    let token = Contract::local(
        "contribution",
        1,
        json!({}),
        BTreeMap::from([("run".into(), descriptor.contribution_member())]),
    )
    .unwrap();

    let source = format!(
        r#"
const token = {}, registry = {}, data = {};
export default {{
    name: 'contribution', uses: [], setup(scope, d) {{
        d.serve(token, (input, self) => {{
            self.registerMethod(registry, 'js', data, 'run');
            return {{
                methods: {{
                    async run(ctx, args) {{
                        {};
                    }}
                }}, state: {{}}
            }};
        }}, {{
            lifetime: 'host'
        }});
    }}
}};
"#,
        serde_json::to_string(token.descriptor()).unwrap(),
        json!({"name":descriptor.registry().name(),"conflict":"error"}),
        json!({"hook":descriptor.fingerprint(),"priority":0,"profile":1}),
        body
    );

    let facet = executor
        .prepare(
            host.id(),
            BundledJsArtifact::new("contribution", source, vec!["default".into()]).unwrap(),
            "default".into(),
        )
        .await
        .unwrap();

    host.install_executable(facet, LoadOptions::default())
        .unwrap()
        .await
        .unwrap();
}

struct UnitCodec;

impl ScopedCodec<()> for UnitCodec {
    fn encode(&self, (): ()) -> Result<NativeValue> {
        panic!("absent positions bypass encode")
    }

    fn decode(&self, _: NativeValue) -> Result<()> {
        panic!("absent positions bypass decode")
    }
}

#[test]
fn absent_series_positions_bypass_codecs_and_discard_actual_foreign_results() {
    run(|driver| {
        async move {
            let (host, executor, manager) = environment(driver).await;

            let descriptor =
                HookDescriptor::new("series", 1, Strategy::Series, None, None, None).unwrap();

            assert_eq!(
                executor
                    .register_hook::<i64, ()>(&descriptor, Rc::new(Integer), Rc::new(UnitCodec))
                    .unwrap_err()
                    .reason,
                "hook_signature"
            );

            executor
                .register_hook(&descriptor, Rc::new(UnitCodec), Rc::new(UnitCodec))
                .unwrap();

            contribution(
                &host,
                &executor,
                &descriptor,
                r#"
if (args.input !== undefined || args.next !== undefined)
    throw Error('presence');
return function discarded() {
};
"#,
            )
            .await;

            let definition = RustHookDefinition::from_descriptor(
                descriptor,
                HookType::absent(),
                HookType::absent(),
            )
            .unwrap();

            host.root(move |ctx| {
                async move { manager.run_dynamic_series(&definition, (), &ctx).await }.boxed_local()
            })
            .await
            .unwrap();

            host.close().await.unwrap();
            assert_eq!(executor.stats().callbacks, 0);
            assert_eq!(executor.stats().contexts, 0);
        }
        .boxed_local()
    });
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
            .ok_or_else(|| RuntimeError::invalid("integer", "integer required"))
    }
}

struct Marker(Cell<usize>);

struct MarkerCodec;

impl ScopedCodec<Rc<Marker>> for MarkerCodec {
    fn encode(&self, value: Rc<Marker>) -> Result<NativeValue> {
        Ok(NativeValue::capability(value))
    }

    fn decode(&self, value: NativeValue) -> Result<Rc<Marker>> {
        value.downcast()
    }
}

#[test]
fn native_hook_values_keep_real_rc_identity_and_exact_root_through_js_and_terminal() {
    run(|driver| {
        async move {
            let (host, executor, manager) = environment(driver).await;

            let native = ValueSchema::Native("marker@1".into());

            let profile = HookType::native("marker@1", |_: &Rc<Marker>| Ok(())).unwrap();

            let descriptor = HookDescriptor::new(
                "native",
                1,
                Strategy::Middleware,
                Some(native.clone()),
                Some(native.clone()),
                Some(TerminalProfile {
                    input: Some(native.clone()),
                    result: Some(native.clone()),
                }),
            )
            .unwrap();

            executor
                .register_hook(&descriptor, Rc::new(MarkerCodec), Rc::new(MarkerCodec))
                .unwrap();

            contribution(
                &host,
                &executor,
                &descriptor,
                "return await args.next.call(args.input);",
            )
            .await;

            let definition =
                RustHookDefinition::from_descriptor(descriptor, profile.clone(), profile.clone())
                    .unwrap();

            let terminal_registry = Registry::new("terminal", ConflictPolicy::Error).unwrap();

            let registered = terminal_registry.clone();

            let marker = Rc::new(Marker(Cell::new(0)));

            let expected = marker.clone();

            let root = Rc::new(Cell::new(None));

            let trace = root.clone();

            let token = Rc::new(
                Contract::local(
                    "terminal",
                    1,
                    json!({}),
                    BTreeMap::from([(
                        "run".into(),
                        Member::Method {
                            args: Some(native.clone()),
                            result: Some(native),
                        },
                    )]),
                )
                .unwrap(),
            );

            host.serve(
                token.clone(),
                Rc::new(move |_, _, d| {
                    d.register_method(&registered, "terminal", json!({}), "run")?;

                    let expected = expected.clone();

                    let trace = trace.clone();

                    Ok(
                        Implementation::new().method("run", move |value: Rc<Marker>, ctx| {
                            assert!(Rc::ptr_eq(&value, &expected));

                            assert_eq!(trace.get(), Some(ctx.root_id()));

                            value.0.set(value.0.get() + 1);

                            async move { Ok(value) }.boxed_local()
                        }),
                    )
                }),
                ServeOptions {
                    lifetime: Lifetime::Host,
                    ..Default::default()
                },
            )
            .unwrap();

            let held = host
                .get(token, json!({}), GetOptions::default())
                .await
                .unwrap();

            let table = host
                .registry(&terminal_registry)
                .unwrap()
                .table(None)
                .unwrap();

            let terminal = HookTerminal::new(
                table.get("terminal").unwrap().operation().unwrap().clone(),
                profile.clone(),
                profile,
            )
            .unwrap();

            let input = marker.clone();

            let result = host
                .root(move |ctx| {
                    root.set(Some(ctx.root_id()));

                    async move {
                        manager
                            .run_dynamic_middleware(&definition, input, &ctx, terminal)
                            .await
                    }
                    .boxed_local()
                })
                .await
                .unwrap();

            assert!(Rc::ptr_eq(&result, &marker));

            assert_eq!(marker.0.get(), 1);

            held.release();

            host.close().await.unwrap();

            assert_eq!(executor.stats().callbacks, 0);

            assert_eq!(executor.stats().capabilities, 0);
        }
        .boxed_local()
    });
}

struct BailCodec;

impl ScopedCodec<BailDecision<bool>> for BailCodec {
    fn encode(&self, value: BailDecision<bool>) -> Result<NativeValue> {
        Ok(NativeValue::portable(match value {
            BailDecision::Continue => json!({"kind":"continue"}),
            BailDecision::Stop(value) => json!({"kind":"stop","value":value}),
        }))
    }

    fn decode(&self, value: NativeValue) -> Result<BailDecision<bool>> {
        serde_json::from_value(value.into_portable()?)
            .map_err(|error| RuntimeError::invalid("bail", error.to_string()))
    }
}

#[test]
fn bail_handler_type_is_distinct_from_semantic_payload_and_false_is_a_stop() {
    run(|driver| {
        async move {
            let (host, executor, manager) = environment(driver).await;

            let descriptor = HookDescriptor::new(
                "bail",
                1,
                Strategy::Bail,
                None,
                Some(ValueSchema::Portable(json!({"type":"boolean"}))),
                None,
            )
            .unwrap();

            executor
                .register_hook(&descriptor, Rc::new(UnitCodec), Rc::new(BailCodec))
                .unwrap();

            contribution(
                &host,
                &executor,
                &descriptor,
                "return {kind:'stop',value:false};",
            )
            .await;

            let definition = RustHookDefinition::from_descriptor(
                descriptor,
                HookType::absent(),
                HookType::<bool>::serde(json!({"type":"boolean"})).unwrap(),
            )
            .unwrap();

            let result = host
                .root(move |ctx| {
                    async move { manager.run_dynamic_bail(&definition, (), &ctx, None).await }
                        .boxed_local()
                })
                .await
                .unwrap();

            assert_eq!(result, BailOutcome::Resolved(false));

            host.close().await.unwrap();

            assert_eq!(executor.stats().callbacks, 0);

            assert_eq!(executor.stats().capabilities, 0);
        }
        .boxed_local()
    });
}
