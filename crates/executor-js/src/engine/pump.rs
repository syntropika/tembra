use super::*;

type Promise = LocalBoxFuture<
    'static,
    std::result::Result<v8::Global<v8::Value>, Box<deno_core::error::JsError>>,
>;
struct PreparedWork {
    evaluation: Evaluation,
    host: u64,
    module: usize,
    export: String,
    reply: oneshot::Sender<Result<ExecutableFacet>>,
    _engine: Rc<Engine>,
}
enum Settled {
    Prepared {
        work: Box<PreparedWork>,
        result: std::result::Result<(), deno_core::error::CoreError>,
    },
    Invoked {
        job: Invocation,
        result: std::result::Result<v8::Global<v8::Value>, Box<deno_core::error::JsError>>,
    },
}

pub(super) async fn pump(engine: Weak<Engine>, mut requests: mpsc::UnboundedReceiver<Request>) {
    let mut pending = FuturesUnordered::<LocalBoxFuture<'static, Settled>>::new();
    poll_fn(|cx| {
        let Some(engine) = engine.upgrade() else {
            return Poll::Ready(());
        };
        engine.pump_waker.register(cx.waker());
        let mut exhausted = true;
        for _ in 0..64 {
            match Pin::new(&mut requests).poll_next(cx) {
                Poll::Ready(Some(request)) => {
                    if !matches!(request, Request::Resolve { .. }) {
                        engine.dequeued();
                    }
                    match request {
                        Request::Resolve {
                            resolver,
                            module,
                            result,
                            owner,
                            lease,
                            completion,
                        } => {
                            let projection = crate::binding::asynchronous::resolve(
                                &engine, &module, resolver, result,
                            );
                            if let Some(completion) = completion
                                && let Err(error) = completion.complete(projection)
                            {
                                engine.report(error);
                            }
                            engine.update(|c| c.physical_jobs -= 1);
                            drop(owner);
                            drop(lease);
                        }
                        Request::Callable(job) => {
                            if let Some(error) = job.refusal.clone() {
                                refuse_callable(&engine, job, error);
                                continue;
                            }
                            if let Err(error) = engine.admit_physical() {
                                refuse_callable(
                                    &engine,
                                    job,
                                    error.with_details(
                                        serde_json::json!({"callablePhase":"admitted"}),
                                    ),
                                );
                                continue;
                            }
                            let (future, completion) = job.call.into_parts();
                            let engine = engine.clone();
                            engine.driver.clone().spawn(
                                async move {
                                    let outcome = std::panic::AssertUnwindSafe(future)
                                        .catch_unwind()
                                        .await
                                        .unwrap_or_else(|_| {
                                            Err(invalid(
                                                "foreign_callback_panic",
                                                "owned native callable future panicked",
                                            ))
                                        });
                                    let result = outcome.map(|value| {
                                        crate::binding::asynchronous::NativeOutput::Native(
                                            job.owner.clone(),
                                            value,
                                        )
                                    });
                                    let request = Request::Resolve {
                                        resolver: job.resolver,
                                        module: job.module,
                                        result,
                                        owner: Some(job.owner),
                                        lease: Some(job.lease),
                                        completion: Some(completion),
                                    };
                                    if engine.resolve(request).is_err() {
                                        engine.update(|c| c.physical_jobs -= 1);
                                    }
                                }
                                .boxed_local(),
                            );
                        }
                        Request::Prepare {
                            host,
                            artifact,
                            export,
                            reply,
                        } => match engine.begin_prepare(host, &artifact, &export) {
                            Ok((module, evaluation)) => {
                                let mut work = Box::new(PreparedWork {
                                    evaluation,
                                    host,
                                    module,
                                    export,
                                    reply,
                                    _engine: engine.clone(),
                                });
                                pending.push(
                                    async move {
                                        let result = work.evaluation.as_mut().await;
                                        Settled::Prepared { work, result }
                                    }
                                    .boxed_local(),
                                );
                            }
                            Err(error) => {
                                let _ = reply.send(Err(error));
                            }
                        },
                        Request::Invoke(job) => {
                            if !job.args.compatible(&engine, &job.callback) {
                                let _ = job.reply.send(Err(invalid(
                                    "foreign_generation",
                                    "callback argument belongs to another runtime generation",
                                )));
                                continue;
                            }

                            if engine.counters.get().physical_jobs
                                >= engine.options.max_physical_jobs
                            {
                                let _ = job.reply.send(Err(limit("physical_jobs")));
                                continue;
                            }
                            let promise: Promise = {
                                let mut runtime = engine.runtime.borrow_mut();
                                let args = job.args.globals();
                                runtime
                                    .call_with_args(&job.callback.function, &args)
                                    .boxed_local()
                            };
                            engine.update(|c| c.physical_jobs += 1);
                            pending.push(
                                async move {
                                    Settled::Invoked {
                                        job,
                                        result: promise.await,
                                    }
                                }
                                .boxed_local(),
                            );
                        }
                    }
                }
                Poll::Ready(None) | Poll::Pending => {
                    exhausted = false;
                    break;
                }
            }
        }
        if exhausted {
            cx.waker().wake_by_ref();
        }
        let event_error = {
            let mut runtime = engine.runtime.borrow_mut();
            match runtime.poll_event_loop(cx, Default::default()) {
                Poll::Ready(Err(error)) => Some(error),
                _ => None,
            }
        };
        if let Some(error) = event_error {
            engine.report(js_error(error));
            cx.waker().wake_by_ref();
        }
        while let Poll::Ready(Some(result)) = Pin::new(&mut pending).poll_next(cx) {
            match result {
                Settled::Prepared { work, result } => {
                    let result = result.map_err(js_error).and_then(|()| {
                        binding::prepare_definition(&engine, work.host, work.module, &work.export)
                    });
                    let _ = work.reply.send(result);
                }
                Settled::Invoked { job, result } => {
                    let result =
                        result
                            .map_err(promise_error)
                            .and_then(|value| match job.conversion {
                                Conversion::Owned => {
                                    let native = crate::binding::native_capability_result(
                                        &engine,
                                        &job.callback.module,
                                        &value,
                                    )?;
                                    Ok(Output::Owned(JsValue {
                                        value,
                                        module: job.callback.module.clone(),
                                        native,
                                        setup: job.callback.slot.setup.clone(),
                                    }))
                                }
                                Conversion::NativeVoid => Ok(Output::Native(Box::new(()))),
                                Conversion::Native(convert) => crate::binding::native_result(
                                    &engine,
                                    &job.callback.module,
                                    value,
                                )
                                .and_then(convert)
                                .map(Output::Native),
                                Conversion::Portable(present) => {
                                    if present {
                                        binding::portable_result(&engine, value)
                                            .map(|value| Output::Portable(Some(value)))
                                    } else {
                                        Ok(Output::Portable(None))
                                    }
                                }
                            });
                    engine.update(|c| c.physical_jobs -= 1);
                    let _ = job.reply.send(result);
                    drop(job.owner);
                    drop(job.lease);
                    drop(job.args);
                    drop(job.callback);
                }
            }
        }
        if requests.is_terminated() && pending.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

/// Runs only after the runtime borrow has ended, including all owned destruction.
pub(crate) fn refuse_callable(engine: &Rc<Engine>, job: CallableJob, cause: RuntimeError) {
    let (future, mut completion) = job.call.into_parts();
    if let Err(error) = completion.refuse(cause.clone()) {
        engine.report(error);
    }
    let projection =
        crate::binding::asynchronous::resolve(engine, &job.module, job.resolver, Err(cause));
    if let Err(error) = completion.complete(projection) {
        engine.report(error);
    }
    drop(future);
}
