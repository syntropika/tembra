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

struct Harness {
    host: Host,
    executor: Executor,
    manager: HookManager,
    definition: RustHookDefinition<i64, i64>,
    terminal: HookTerminal<i64, i64>,
    calls: Rc<Cell<usize>>,
    closes: Rc<Cell<usize>>,
    expected_closes: usize,
}

impl Harness {
    async fn new(driver: Rc<TokioDriver>, body: &str, terminal_error: bool) -> Self {
        Self::configured(
            driver,
            body,
            Rc::new(Integer),
            Rc::new(move |value, _| {
                async move {
                    if terminal_error {
                        Err(RuntimeError::invalid("terminal", "terminal failed"))
                    } else {
                        Ok(value * 2)
                    }
                }
                .boxed_local()
            }),
        )
        .await
    }

    async fn configured(
        driver: Rc<TokioDriver>,
        body: &str,
        output: Rc<dyn ScopedCodec<i64>>,
        terminal_work: Rc<dyn Fn(i64, Context) -> LocalBoxFuture<'static, Result<i64>>>,
    ) -> Self {
        Self::with_codecs(driver, body, Rc::new(Integer), output, terminal_work, false).await
    }

    async fn with_codecs(
        driver: Rc<TokioDriver>,
        body: &str,
        input_codec: Rc<dyn ScopedCodec<i64>>,
        output: Rc<dyn ScopedCodec<i64>>,
        terminal_work: Rc<dyn Fn(i64, Context) -> LocalBoxFuture<'static, Result<i64>>>,
        native_output: bool,
    ) -> Self {
        Self::with_options(
            driver,
            body,
            input_codec,
            output,
            terminal_work,
            native_output,
            ExecutorOptions::default(),
        )
        .await
    }

    async fn with_options(
        driver: Rc<TokioDriver>,
        body: &str,
        input_codec: Rc<dyn ScopedCodec<i64>>,
        output: Rc<dyn ScopedCodec<i64>>,
        terminal_work: Rc<dyn Fn(i64, Context) -> LocalBoxFuture<'static, Result<i64>>>,
        native_output: bool,
        options: ExecutorOptions,
    ) -> Self {
        let host = Host::new(
            HostOptions::new("hooks", json!("principal")),
            driver.clone(),
        );

        let expected_closes = usize::from(options.max_physical_jobs >= 3);
        let executor = Executor::new(driver.clone(), options).unwrap();

        let profile = ValueSchema::Portable(json!({"type":"integer"}));

        let output_profile = if native_output {
            ValueSchema::Native("projection@1".into())
        } else {
            profile.clone()
        };

        let descriptor = HookDescriptor::new(
            "test",
            1,
            Strategy::Middleware,
            Some(profile.clone()),
            Some(output_profile.clone()),
            Some(TerminalProfile {
                input: Some(profile.clone()),
                result: Some(output_profile.clone()),
            }),
        )
        .unwrap();

        executor
            .register_hook(&descriptor, input_codec, output)
            .unwrap();

        let input = HookType::<i64>::serde(json!({"type":"integer"})).unwrap();

        let handler_type = if native_output {
            HookType::native("projection@1", |_: &i64| Ok(())).unwrap()
        } else {
            input.clone()
        };

        let definition = RustHookDefinition::from_descriptor(
            descriptor.clone(),
            input.clone(),
            handler_type.clone(),
        )
        .unwrap();

        let token = Rc::new(
            Contract::local(
                "contribution",
                1,
                json!({}),
                BTreeMap::from([("run".into(), descriptor.contribution_member())]),
            )
            .unwrap(),
        );

        let terminal_token = Rc::new(
            Contract::local(
                "terminal",
                1,
                json!({}),
                BTreeMap::from([(
                    "run".into(),
                    Member::Method {
                        args: Some(profile.clone()),
                        result: Some(output_profile),
                    },
                )]),
            )
            .unwrap(),
        );

        let terminal_registry = Registry::new("terminal", ConflictPolicy::Error).unwrap();

        let registry = terminal_registry.clone();

        let calls = Rc::new(Cell::new(0));

        let effects = calls.clone();

        host.serve(
            terminal_token,
            Rc::new(move |_, _, d| {
                d.register_method(&registry, "terminal", json!({}), "run")?;

                let effects = effects.clone();

                let terminal_work = terminal_work.clone();

                Ok(
                    Implementation::new().method("run", move |value: i64, context| {
                        assert_eq!(context.code_version(), 1);

                        assert_eq!(context.principal(), &json!("principal"));

                        effects.set(effects.get() + 1);

                        terminal_work(value, context)
                    }),
                )
            }),
            ServeOptions {
                lifetime: Lifetime::Host,
                ..Default::default()
            },
        )
        .unwrap();

        let closes = Rc::new(Cell::new(0));

        let sink = Rc::new(
            Contract::local(
                "close",
                1,
                json!({}),
                BTreeMap::from([(
                    "run".into(),
                    Member::Method {
                        args: None,
                        result: None,
                    },
                )]),
            )
            .unwrap(),
        );

        let count = closes.clone();

        host.serve(
            sink.clone(),
            Rc::new(move |_, _, _| {
                let count = count.clone();

                Ok(Implementation::new().json_method("run", move |_, _| {
                    count.set(count.get() + 1);

                    async { Ok(None) }.boxed_local()
                }))
            }),
            ServeOptions::default(),
        )
        .unwrap();

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

        let source = format!(
            r#"
const token = {token}, registry = {registry}, data = {data}, sink = {sink};
export default {{
    name: 'middleware', uses: [], setup(scope, d) {{
        d.serve(token, (input, self) => {{
            const close = self.get(sink, {{}});
            if ({retain_close}) self.resource('held', {{
                open: () => 1, close: () => scope.root(ctx => close.run(ctx))
            }});
            self.registerMethod(registry, 'js', data, 'run');
            return {{
                methods: {{
                    async run(ctx, args) {{
                        {body};
                    }}
                }}, state: {{}}
            }};
        }}, {{
            lifetime: 'host'
        }});
    }}
}};
"#,
            retain_close = expected_closes != 0,
            token = serde_json::to_string(token.descriptor()).unwrap(),
            sink = serde_json::to_string(sink.descriptor()).unwrap(),
            registry = json!({"name":descriptor.registry().name(),"conflict":"error"}),
            data = json!({"hook":descriptor.fingerprint(),"priority":0,"profile":1})
        );

        let facet = executor
            .prepare_for_host(
                &host,
                BundledJsArtifact::new("middleware", source, vec!["default".into()]).unwrap(),
                "default".into(),
            )
            .await
            .unwrap();

        host.install_executable(facet, LoadOptions::default())
            .unwrap()
            .await
            .unwrap();

        let scope = scope.borrow().clone().unwrap();

        let manager = HookManager::new(scope, driver, HookManagerOptions::default()).unwrap();

        let table = host
            .registry(&terminal_registry)
            .unwrap()
            .table(None)
            .unwrap();

        let operation = table.get("terminal").unwrap().operation().unwrap().clone();

        let terminal = HookTerminal::new(operation, input.clone(), handler_type).unwrap();

        Self {
            host,
            executor,
            manager,
            definition,
            terminal,
            calls,
            closes,
            expected_closes,
        }
    }

    async fn invoke(&self, input: i64) -> Result<i64> {
        let manager = self.manager.clone();

        let definition = self.definition.clone();

        let terminal = self.terminal.clone();

        self.host
            .root(move |context| {
                async move {
                    manager
                        .run_dynamic_middleware(&definition, input, &context, terminal)
                        .await
                }
                .boxed_local()
            })
            .await
    }

    async fn close(self) {
        self.host.close().await.unwrap();

        assert_eq!(self.executor.stats().physical_jobs, 0);

        assert_eq!(self.executor.stats().contexts, 0);

        assert_eq!(self.executor.stats().callbacks, 0);

        assert_eq!(self.executor.stats().capabilities, 0);

        assert_eq!(self.closes.get(), self.expected_closes);
    }
}

#[test]
fn actual_typed_middleware_enters_js_and_managed_rust_terminal() {
    run(|driver| {
        async move {
            let h = Harness::new(
                driver,
                r#"
if (Object.getPrototypeOf(args) !== null || !Object.isFrozen(args) || !Object.isFrozen(args.next))
    throw Error('record');
return (await args.next.call(args.input + 1)) + 3;
"#,
                false,
            )
            .await;

            assert_eq!(h.invoke(5).await.unwrap(), 15);
            assert_eq!(h.calls.get(), 1);
            h.close().await;
        }
        .boxed_local()
    });
}

#[test]
fn explicit_recovery_is_required_and_secondary_misuse_does_not_override_primary() {
    for (body, error, value, calls) in [
        ("return 17;", None, 17, 0),
        (
            r#"
const value = await args.next.call(args.input);
try {
    await args.next.call(9);
    throw Error('duplicate accepted');
}
catch (e) {
    if (e.reason !== 'hook_next_used')
        throw e;
}
return value;
"#,
            None,
            10,
            1,
        ),
        (
            r#"
try {
    await args.next.call('invalid');
}
catch (e) {
    if (e.reason !== 'validation')
        throw e;
}
try {
    await args.next.call(9);
}
catch (e) {
    if (e.reason !== 'hook_next_used')
        throw e;
}
return 99;
"#,
            Some("validation"),
            0,
            0,
        ),
        (
            "try{await args.next.call(args.input);}catch(e){return 99;}",
            Some("terminal"),
            0,
            1,
        ),
        (
            "try{await args.next.call(args.input);}catch(e){return await args.next.recover(99);}",
            None,
            99,
            1,
        ),
    ] {
        run(move |driver| {
            async move {
                let h = Harness::new(
                    driver,
                    body,
                    calls == 1 && body.contains("recover")
                        || body.contains("return 99;}") && !body.contains("invalid"),
                )
                .await;

                let result = h.invoke(5).await;

                if let Some(reason) = error {
                    assert_eq!(result.unwrap_err().reason, reason);
                } else {
                    assert_eq!(result.unwrap(), value);
                }

                assert_eq!(h.calls.get(), calls);

                h.close().await;
            }
            .boxed_local()
        });
    }
}

#[test]
fn detached_next_is_physically_joined_and_expired_callable_cannot_start_again() {
    run(|driver| {
        async move {
            use futures::{
                channel::oneshot,
                future::{Either, select},
            };

            let (send, receive) = oneshot::channel();
            let gate = Rc::new(RefCell::new(Some(receive)));

            let h = Harness::configured(
                driver,
                r#"
if (globalThis.saved) {
    try {
        await globalThis.saved.call(77);
        throw Error('late accepted');
    }
    catch (e) {
        if (e.reason !== 'expired')
            throw e;
    }
    return 21;
}
globalThis.saved = args.next;
void args.next.call(args.input);
return 19;
"#,
                Rc::new(Integer),
                Rc::new(move |value, _| {
                    let gate = gate.borrow_mut().take().unwrap();
                    async move {
                        gate.await.unwrap();
                        Ok(value * 2)
                    }
                    .boxed_local()
                }),
            )
            .await;

            let pending = h.invoke(5).boxed_local();

            let pending = match select(
                pending,
                tokio::time::sleep(Duration::from_millis(10)).boxed_local(),
            )
            .await
            {
                Either::Right(((), pending)) => pending,
                Either::Left(_) => panic!("detached downstream was not joined"),
            };

            assert_eq!(h.calls.get(), 1);
            assert!(h.executor.stats().physical_jobs > 0);

            send.send(()).unwrap();
            assert_eq!(pending.await.unwrap(), 19);

            assert_eq!(h.invoke(5).await.unwrap(), 21);
            assert_eq!(h.calls.get(), 1);

            h.close().await;
        }
        .boxed_local()
    });
}

struct PoisonProjection(Rc<Cell<usize>>);

impl ScopedCodec<i64> for PoisonProjection {
    fn encode(&self, _: i64) -> Result<NativeValue> {
        let effects = self.0.clone();

        let then = NativeCallable::new(Rc::new(move |_| {
            effects.set(effects.get() + 1);

            async { Ok(NativeValue::absent()) }.boxed_local()
        }));

        Ok(NativeValue::record(NativeRecord::new([(
            "then".into(),
            NativeValue::callable(then),
        )])?))
    }

    fn decode(&self, value: NativeValue) -> Result<i64> {
        Integer.decode(value)
    }
}

#[test]
fn final_projection_failure_is_sticky_without_executing_then_or_hiding_handler_error() {
    for throws in [false, true] {
        run(move |driver| {
            async move {
                let effects = Rc::new(Cell::new(0));

                let body = if throws {
                    "try{await args.next.call(args.input);}catch(e){}throw Error('handler-first');"
                } else {
                    "try{await args.next.call(args.input);}catch(e){}return 77;"
                };

                let h = Harness::with_codecs(
                    driver,
                    body,
                    Rc::new(Integer),
                    Rc::new(PoisonProjection(effects.clone())),
                    Rc::new(|value, _| async move { Ok(value * 2) }.boxed_local()),
                    true,
                )
                .await;

                let error = h.invoke(5).await.unwrap_err();

                if throws {
                    assert!(error.message.contains("handler-first"));
                } else {
                    assert_eq!(error.reason, "native_thenable");
                }

                assert_eq!(effects.get(), 0);

                assert_eq!(h.calls.get(), 1);

                h.close().await;
            }
            .boxed_local()
        });
    }
}

#[test]
fn dropped_outer_waiter_and_host_close_join_detached_terminal_before_resource_close() {
    run(|driver| {
        async move {
            use futures::{
                channel::oneshot,
                future::{Either, select},
            };

            let (started, started_rx) = oneshot::channel();

            let (gate, gate_rx) = oneshot::channel();

            let channels = Rc::new(RefCell::new(Some((started, gate_rx))));

            let h = Harness::configured(
                driver,
                "void args.next.call(args.input);return 19;",
                Rc::new(Integer),
                Rc::new(move |value, _| {
                    let (started, wait) = channels.borrow_mut().take().unwrap();

                    async move {
                        started.send(()).unwrap();

                        wait.await.unwrap();

                        Ok(value * 2)
                    }
                    .boxed_local()
                }),
            )
            .await;

            let call = h.invoke(5).boxed_local();

            let Either::Right((Ok(()), pending)) = select(call, started_rx).await else {
                panic!("terminal must start");
            };

            drop(pending);

            assert_eq!(h.closes.get(), 0);

            let close = h.host.close().boxed_local();

            let Either::Right(((), close)) = select(
                close,
                tokio::time::sleep(Duration::from_millis(10)).boxed_local(),
            )
            .await
            else {
                panic!("close skipped physical work");
            };

            assert_eq!(h.closes.get(), 0);

            gate.send(()).unwrap();

            close.await.unwrap();

            h.close().await;
        }
        .boxed_local()
    });
}

struct ExhaustProjection;

impl ScopedCodec<i64> for ExhaustProjection {
    fn encode(&self, _: i64) -> Result<NativeValue> {
        Ok(NativeValue::record(NativeRecord::new((0..1500).map(
            |index| {
                (
                    format!("field{index}"),
                    NativeValue::capability(Rc::new(index)),
                )
            },
        ))?))
    }

    fn decode(&self, value: NativeValue) -> Result<i64> {
        Integer.decode(value)
    }
}

#[test]
fn output_capability_quota_failure_stays_sticky_after_js_catches_it() {
    run(|driver| {
        async move {
            let h = Harness::with_codecs(
                driver,
                r#"
try {
    await args.next.call(args.input);
}
catch (e) {
    if (e.reason !== 'capabilities')
        throw e;
}
return 77;
"#,
                Rc::new(Integer),
                Rc::new(ExhaustProjection),
                Rc::new(|value, _| async move { Ok(value) }.boxed_local()),
                true,
            )
            .await;

            assert_eq!(h.invoke(5).await.unwrap_err().reason, "capabilities");
            assert_eq!(h.calls.get(), 1);
            assert_eq!(h.executor.stats().contexts, 0);
            h.close().await;
        }
        .boxed_local()
    });
}

#[test]
fn invalid_recovery_can_retry_but_early_and_repeated_recovery_are_refused() {
    for (body, fails) in [
        (
            r#"
try {
    await args.next.recover(1);
}
catch (e) {
    if (e.reason !== 'hook_recovery')
        throw e;
}
return await args.next.call(args.input);
"#,
            false,
        ),
        (
            r#"
try {
    await args.next.call(args.input);
}
catch (e) {
    try {
        await args.next.recover('invalid');
    }
    catch (e) {
        if (e.reason !== 'validation')
            throw e;
    }
    const result = await args.next.recover(99);
    try {
        await args.next.recover(11);
    }
    catch (e) {
        if (e.reason !== 'hook_recovery')
            throw e;
    }
    return result;
}
"#,
            true,
        ),
    ] {
        run(move |driver| {
            async move {
                let h = Harness::new(driver, body, fails).await;

                assert_eq!(h.invoke(5).await.unwrap(), if fails { 99 } else { 10 });

                h.close().await;
            }
            .boxed_local()
        });
    }
}

#[test]
fn callable_then_guard_uses_captured_intrinsics() {
    run(|driver| {
        async move {
            let effects = Rc::new(Cell::new(0));

            let body = r#"
const descriptor = Object.getOwnPropertyDescriptor, prototype = Object.getPrototypeOf;
Object.getOwnPropertyDescriptor = () => undefined;
Object.getPrototypeOf = () => null;
try {
    try {
        await args.next.call(args.input);
    }
    catch (e) {
        if (e.reason !== 'native_thenable')
            throw e;
    }
}
finally {
    Object.getOwnPropertyDescriptor = descriptor;
    Object.getPrototypeOf = prototype;
}
return 77;
"#;

            let h = Harness::with_codecs(
                driver,
                body,
                Rc::new(Integer),
                Rc::new(PoisonProjection(effects.clone())),
                Rc::new(|value, _| async move { Ok(value) }.boxed_local()),
                true,
            )
            .await;

            assert_eq!(h.invoke(5).await.unwrap_err().reason, "native_thenable");
            assert_eq!(effects.get(), 0);
            h.close().await;
        }
        .boxed_local()
    });
}

#[test]
fn actual_cancellation_reaches_started_terminal_and_keeps_cooperative_late_result() {
    run(|driver| {
        async move {
            use futures::{
                channel::oneshot,
                future::{Either, select},
            };

            let (started, started_rx) = oneshot::channel();
            let started = Rc::new(RefCell::new(Some(started)));

            let h = Harness::configured(
                driver,
                r#"
const value = await args.next.call(args.input);
try {
    await args.next.call(99);
    throw Error('cancelled replay');
}
catch (e) {
    if (e.code !== 'aborted')
        throw e;
}
return value;
"#,
                Rc::new(Integer),
                Rc::new(move |value, ctx| {
                    let started = started.borrow_mut().take().unwrap();
                    async move {
                        started.send(()).unwrap();
                        ctx.cancellation().cancelled().await;
                        Ok(value * 2)
                    }
                    .boxed_local()
                }),
            )
            .await;

            let manager = h.manager.clone();
            let definition = h.definition.clone();
            let terminal = h.terminal.clone();
            let context = Rc::new(RefCell::new(None));
            let captured = context.clone();

            let call = h
                .host
                .root(move |ctx| {
                    *captured.borrow_mut() = Some(ctx.clone());
                    async move {
                        manager
                            .run_dynamic_middleware(&definition, 5, &ctx, terminal)
                            .await
                    }
                    .boxed_local()
                })
                .boxed_local();

            let Either::Right((Ok(()), call)) = select(call, started_rx).await else {
                panic!("terminal must start");
            };

            context
                .borrow()
                .as_ref()
                .unwrap()
                .cancel("cancelled by caller");
            assert_eq!(call.await.unwrap(), 10);
            assert_eq!(h.calls.get(), 1);
            context.borrow_mut().take();
            h.close().await;
        }
        .boxed_local()
    });
}

struct PanickingProjection(Cell<bool>);

impl ScopedCodec<i64> for PanickingProjection {
    fn encode(&self, value: i64) -> Result<NativeValue> {
        assert!(!self.0.replace(false), "hook output codec panic");

        Integer.encode(value)
    }

    fn decode(&self, value: NativeValue) -> Result<i64> {
        Integer.decode(value)
    }
}

#[test]
fn next_output_codec_panic_is_a_recoverable_attempt_failure() {
    run(|driver| {
        async move {
            let h = Harness::configured(
                driver,
                r#"
try {
    await args.next.call(args.input);
}
catch (e) {
    if (e.reason !== 'codec_panic')
        throw e;
    return await args.next.recover(99);
}
"#,
                Rc::new(PanickingProjection(Cell::new(true))),
                Rc::new(|value, _| async move { Ok(value) }.boxed_local()),
            )
            .await;

            assert_eq!(h.invoke(5).await.unwrap(), 99);
            assert_eq!(h.calls.get(), 1);
            h.close().await;
        }
        .boxed_local()
    });
}

struct PermissiveInput {
    invalid_encode: bool,
}

impl ScopedCodec<i64> for PermissiveInput {
    fn encode(&self, value: i64) -> Result<NativeValue> {
        Ok(NativeValue::portable(if self.invalid_encode {
            json!("invalid")
        } else {
            json!(value)
        }))
    }

    fn decode(&self, _: NativeValue) -> Result<i64> {
        Ok(5)
    }
}

#[test]
fn nested_portable_hook_positions_validate_before_permissive_codecs_and_terminal_effects() {
    for invalid_encode in [false, true] {
        run(move |driver| {
            async move {
                let h = Harness::with_codecs(
                    driver,
                    r#"
try {
    await args.next.call('invalid');
}
catch (e) {
}
try {
    await args.next.call(5);
}
catch (e) {
    if (e.reason !== 'hook_next_used')
        throw e;
}
return 99;
"#,
                    Rc::new(PermissiveInput { invalid_encode }),
                    Rc::new(Integer),
                    Rc::new(|value, _| async move { Ok(value) }.boxed_local()),
                    false,
                )
                .await;

                let error = h.invoke(5).await.unwrap_err();

                assert_eq!(error.reason, "validation");

                assert_eq!(h.calls.get(), 0);

                h.close().await;
            }
            .boxed_local()
        });
    }
}

#[test]
fn queued_next_and_recovery_are_admitted_before_async_handler_seals() {
    for (body, terminal_error, expected) in [
        (
            "await null; void args.next.call(args.input); return 19;",
            false,
            19,
        ),
        (
            r#"
await null;
try { await args.next.call(args.input); }
catch (_) { void args.next.recover(99); return 99; }
return 0;
"#,
            true,
            99,
        ),
        (
            r#"
await null;
const primary = args.next.call(args.input);
for (let index = 0; index < 64; index++) {
    let pending;
    try { pending = args.next.call(args.input); }
    catch (_) { throw Error('queue refusal threw synchronously'); }
    if (!(pending instanceof Promise)) throw Error('Promise required');
    void pending.catch(error => {
        if (error.reason !== 'hook_next_used' && error.reason !== 'queued_jobs') throw error;
        if (error.reason === 'queued_jobs' && error.details.callablePhase !== 'ingress')
            throw Error('wrong callable admission phase');
    });
}
return await primary;
"#,
            false,
            10,
        ),
        (
            r#"
await null;
for (let index = 0; index < 63; index++) {
    void args.next.recover(0).catch(() => {});
}
void args.next.call(args.input);
return 19;
"#,
            false,
            19,
        ),
    ] {
        run(move |driver| {
            async move {
                let h = Harness::new(driver, body, terminal_error).await;
                assert_eq!(h.invoke(5).await.unwrap(), expected);
                assert_eq!(h.calls.get(), 1);
                h.close().await;
            }
            .boxed_local()
        });
    }
}

#[test]
fn saturated_secondary_call_does_not_poison_the_accepted_primary() {
    run(|driver| {
        async move {
            let h = Harness::with_options(
                driver,
                r#"
await null;
const primary = args.next.call(args.input);
const secondary = args.next.call(args.input);
try { await secondary; }
catch (error) {
    if (error.reason !== 'physical_jobs' || error.details.callablePhase !== 'admitted') throw error;
}
return await primary;
"#,
                Rc::new(Integer),
                Rc::new(Integer),
                Rc::new(|value, _| {
                    async move {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        Ok(value * 2)
                    }
                    .boxed_local()
                }),
                false,
                ExecutorOptions {
                    max_physical_jobs: 2,
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(h.invoke(5).await.unwrap(), 10);
            assert_eq!(h.calls.get(), 1);
            h.close().await;
        }
        .boxed_local()
    });
}

#[test]
fn accepted_primary_physical_refusal_can_be_explicitly_recovered() {
    run(|driver| {
        async move {
            let h = Harness::with_options(
                driver,
                r#"
await null;
const blocker = scope.root(() => 1).catch(error => {
    if (error.reason !== 'physical_jobs') throw error;
});
try { await args.next.call(args.input); }
catch (error) {
    if (error.reason !== 'physical_jobs' || error.details.callablePhase !== 'admitted') throw error;
    await blocker;
    return await args.next.recover(99);
}
throw Error('expected bounded physical refusal');
"#,
                Rc::new(Integer),
                Rc::new(Integer),
                Rc::new(|value, _| async move { Ok(value * 2) }.boxed_local()),
                false,
                ExecutorOptions {
                    max_physical_jobs: 2,
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(h.invoke(5).await.unwrap(), 99);
            assert_eq!(h.calls.get(), 0);
            h.close().await;
        }
        .boxed_local()
    });
}
