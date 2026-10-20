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

struct Action {
    cell: tembra_core::Cell,
    context: Rc<RefCell<Option<Context>>>,
    prepared: Rc<Cell<usize>>,
    completed: Rc<Cell<usize>>,
    polled: Rc<Cell<usize>>,
}

impl ScopedCodec<i64> for Action {
    fn encode(&self, value: i64) -> Result<NativeValue> {
        let context = self.context.borrow().clone().unwrap();

        let cell = self.cell.clone();

        let prepared = self.prepared.clone();

        let completed = self.completed.clone();

        let polled = self.polled.clone();

        let callable = NativeCallable::deferred(Rc::new(move |decode| {
            prepared.set(prepared.get() + 1);

            assert!(
                value != 0 || prepared.get() != 1,
                "preparation unwind probe"
            );

            // This changes a source whose synchronous derived callback is real JavaScript in
            // the same borrowed engine. The scoped evaluator must reenter safely.
            cell.replace(&context, json!(2), ReplaceOptions::default())
                .unwrap();

            let polled = polled.clone();

            let cell = cell.clone();

            let context = context.clone();

            let completed = completed.clone();

            NativeCall::with_completion(
                async move {
                    polled.set(polled.get() + 1);

                    assert!(
                        value != -1 || polled.get() != 1,
                        "owned future unwind probe"
                    );

                    let value = Integer.decode(decode()?)?;

                    Integer.encode(value)
                }
                .boxed_local(),
                Box::new(move |projection| {
                    assert!(
                        projection.is_ok(),
                        "prepared refusal is not projection failure"
                    );
                    // Completion must run after the runtime borrow ends, including quota refusal.
                    cell.replace(&context, json!(3), ReplaceOptions::default())
                        .unwrap();

                    completed.set(completed.get() + 1);

                    assert!(value != -2, "completion unwind probe");
                }),
            )
        }));

        Ok(NativeValue::record(NativeRecord::new([
            ("value".into(), NativeValue::portable(json!(value))),
            ("action".into(), NativeValue::callable(callable)),
        ])?))
    }

    fn decode(&self, _: NativeValue) -> Result<i64> {
        Err(RuntimeError::invalid("input", "provider-only codec"))
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

#[test]
fn deferred_preparation_reenters_js_derive_and_refusal_completion_runs_outside_runtime() {
    for (capacity, input, preparations, finishes, polls, errors) in [
        (1, 7, 1, 1, 0, 0),
        (128, 7, 1, 1, 1, 0),
        (128, 0, 2, 1, 1, 0),
        (128, -1, 2, 2, 2, 0),
        (128, -2, 1, 1, 1, 1),
    ] {
        run(move |driver| {
            async move {
                let host = Host::new(HostOptions::new("callable", json!("host")), driver.clone());

                let executor = Executor::new(
                    driver,
                    ExecutorOptions {
                        max_physical_jobs: capacity,
                        max_queued_jobs: 1,
                        ..Default::default()
                    },
                )
                .unwrap();

                let state = Rc::new(
                    Contract::local(
                        "state",
                        1,
                        json!({}),
                        BTreeMap::from([(
                            "count".into(),
                            Member::State {
                                schema: json!({"type":"integer"}),
                            },
                        )]),
                    )
                    .unwrap(),
                );

                let cell = Rc::new(RefCell::new(None));
                let captured = cell.clone();

                host.serve(
                    state.clone(),
                    Rc::new(move |_, _, declare| {
                        let count = declare.state(
                            "count",
                            StateDeclaration::new(json!({"type":"integer"}), json!(1)),
                        )?;
                        *captured.borrow_mut() = Some(count.clone());
                        Ok(Implementation::new().state("count", count.readable()))
                    }),
                    ServeOptions {
                        lifetime: Lifetime::Host,
                        ..Default::default()
                    },
                )
                .unwrap();

                let held = host
                    .get(state.clone(), json!({}), GetOptions::default())
                    .await
                    .unwrap();

                let context = Rc::new(RefCell::new(None));
                let prepared = Rc::new(Cell::new(0));
                let completed = Rc::new(Cell::new(0));
                let polled = Rc::new(Cell::new(0));

                let codec = Action {
                    cell: cell.borrow().clone().unwrap(),
                    context: context.clone(),
                    prepared: prepared.clone(),
                    completed: completed.clone(),
                    polled: polled.clone(),
                };

                let wrapper = Rc::new(
                    move |args, ctx: Context, invoke: tembra_hooks::MethodHandler<i64, i64>| {
                        *context.borrow_mut() = Some(ctx.clone());
                        let context = context.clone();
                        async move {
                            let result = invoke(args, ctx).await;
                            context.borrow_mut().take();
                            result
                        }
                        .boxed_local()
                    },
                );

                executor
                    .register_native_method(
                        NativeMethodSignature::from_schemas(
                            Some(ValueSchema::Native("action@1".into())),
                            Some(ValueSchema::Portable(json!({"type":"integer"}))),
                        )
                        .unwrap(),
                        NativeMethodCodec::new(Rc::new(codec), Rc::new(Integer))
                            .with_wrapper(wrapper),
                    )
                    .unwrap();

                let token = Rc::new(
                    Contract::local(
                        "trigger",
                        1,
                        json!({}),
                        BTreeMap::from([(
                            "run".into(),
                            Member::Method {
                                args: Some(ValueSchema::Native("action@1".into())),
                                result: Some(ValueSchema::Portable(json!({"type":"integer"}))),
                            },
                        )]),
                    )
                    .unwrap(),
                );

                let source = format!(
                    r#"
const source = {state}, token = {token};
export default {{
    name: 'callable', uses: [], setup(scope, d) {{
        d.serve(token, (input, self) => {{
            const heldSource = self.get(source, {{}});
            const derived = self.derive([
                heldSource.count
            ], {{
                type: 'integer'
            }}, ([s]) => s.value * 2);
            return {{
                methods: {{
                    async run(ctx, args) {{
                        try {{
                            await args.action(args.value);
                        }}
                        catch (e) {{
                            if (e.reason === 'foreign_callback_panic')
                                await args.action(args.value);
                            else if (e.reason !== 'physical_jobs')
                                throw e;
                        }}
                        return derived.value;
                    }}
                }}, state: {{}}
            }};
        }});
    }}
}};
"#,
                    state = serde_json::to_string(state.descriptor()).unwrap(),
                    token = serde_json::to_string(token.descriptor()).unwrap()
                );

                let facet = executor
                    .prepare(
                        host.id(),
                        BundledJsArtifact::new("callable", source, vec!["default".into()]).unwrap(),
                        "default".into(),
                    )
                    .await
                    .unwrap();

                host.install_executable(facet, LoadOptions::default())
                    .unwrap()
                    .await
                    .unwrap();

                let reference = host
                    .get(token, json!({}), GetOptions::default())
                    .await
                    .unwrap();

                assert_eq!(
                    reference
                        .call::<i64, i64>("run", input, None)
                        .await
                        .unwrap(),
                    6
                );

                assert_eq!(prepared.get(), preparations);
                assert_eq!(completed.get(), finishes);
                assert_eq!(polled.get(), polls);

                let diagnostics = executor.take_errors();
                assert_eq!(diagnostics.len(), errors);
                assert!(
                    diagnostics
                        .iter()
                        .all(|error| error.reason == "foreign_callback_panic")
                );

                assert_eq!(reference.call::<i64, i64>("run", 7, None).await.unwrap(), 6);
                assert_eq!(executor.stats().queued_jobs, 0);
                assert_eq!(executor.stats().physical_jobs, 0);

                reference.release();
                held.release();
                host.close().await.unwrap();
                assert_eq!(executor.stats().physical_jobs, 0);
                assert_eq!(executor.stats().contexts, 0);
                assert_eq!(executor.stats().callbacks, 0);
            }
            .boxed_local()
        });
    }
}
