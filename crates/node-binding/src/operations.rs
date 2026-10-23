use crate::{callbacks::NodeValue, factory::*, handles::*, session::Session};
use futures::{FutureExt, future::LocalBoxFuture};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc, time::Duration};
use tembra_core::*;

fn absent() -> Value {
    json!({"$undefined": true})
}
fn invalid() -> RuntimeError {
    RuntimeError::invalid("binding", "operation does not match issued handle")
}
fn option_state(value: &Value) -> Result<GetOptions> {
    let options = parse(value)?;
    let fields = options.as_object().ok_or_else(invalid)?;
    if fields.keys().any(|key| key != "state") {
        return Err(invalid());
    }
    let state = match fields.get("state") {
        None => true,
        Some(Value::Bool(v)) => *v,
        _ => return Err(invalid()),
    };
    Ok(GetOptions {
        state,
        ..GetOptions::default()
    })
}
pub(crate) fn context(session: &Session, token: &Value) -> Result<IssuedContext> {
    match session.entry(handle(token)?)? {
        Entry::Context(entry) => {
            entry.context.check(session.host.id())?;
            Ok(entry)
        }
        _ => Err(RuntimeError::invalid(
            "foreign_ctx",
            "expected issued managed Context",
        )),
    }
}
fn source(session: &Session, value: &Value) -> Result<Readable> {
    match session.entry(handle(value)?)? {
        Entry::State(s) => Ok(s),
        _ => Err(invalid()),
    }
}
fn resource(session: &Session, value: &Value) -> Result<Resource<NodeValue>> {
    match session.entry(handle(value)?)? {
        Entry::Resource(s) => s.upgrade(),
        _ => Err(invalid()),
    }
}
fn reference(session: &Session, value: &Value) -> Result<Reference> {
    match session.entry(handle(value)?)? {
        Entry::Reference(s) => Ok(s),
        _ => Err(invalid()),
    }
}
fn removal(session: &Session, action: Box<dyn Fn()>) -> Result<Value> {
    session.issue(Entry::Subscription(Rc::new(Removal(RefCell::new(Some(
        action,
    ))))))
}
fn bool_arg(value: &Value) -> Result<bool> {
    value.as_bool().ok_or_else(invalid)
}
fn number(value: &Value) -> Result<u64> {
    value
        .as_u64()
        .filter(|n| *n > 0 && *n <= 9_007_199_254_740_991)
        .ok_or_else(invalid)
}
fn core_signal(session: &Session, signal: Signal) -> Result<Value> {
    session.issue(Entry::Signal(signal))
}
fn report(report: ReloadReport) -> Value {
    json!({"codeVersion":report.current_version(), "previousVersion":report.previous_version(), "changed":report.changed(), "removed":report.removed(), "preparedObjects":report.prepared_objects(), "gatedObjects":report.gated_objects(), "lingeringPhysical":report.lingering_physical()})
}
fn reload_options(value: &Value) -> Result<ReloadOptions> {
    let value = parse(value)?;
    let fields = value.as_object().ok_or_else(invalid)?;
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "remove" | "gateDeadline" | "retireDeadline" | "taskStopDeadline"
        )
    }) {
        return Err(invalid());
    }
    let mut result = ReloadOptions::default();
    if let Some(remove) = fields.get("remove") {
        result.remove = remove
            .as_array()
            .ok_or_else(invalid)?
            .iter()
            .map(|v| text(v).map(str::to_owned))
            .collect::<Result<_>>()?;
    }
    for (key, target) in [
        ("gateDeadline", &mut result.gate_deadline),
        ("retireDeadline", &mut result.retire_deadline),
        ("taskStopDeadline", &mut result.task_stop_deadline),
    ] {
        if let Some(value) = fields.get(key) {
            *target = Duration::from_millis(number(value)?);
        }
    }
    Ok(result)
}

fn take_candidates(session: &Session, value: &Value) -> Result<Vec<ExecutableFacet>> {
    let array = value.as_array().ok_or_else(invalid)?;
    let mut ids = std::collections::BTreeSet::new();
    let candidates = array
        .iter()
        .map(|wire| {
            let id = handle(wire)?;
            if !ids.insert(id) {
                return Err(invalid());
            }
            match session.entry(id)? {
                Entry::Candidate(candidate) if candidate.borrow().is_some() => Ok(candidate),
                _ => Err(invalid()),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    candidates
        .into_iter()
        .map(|candidate| candidate.borrow_mut().take().ok_or_else(invalid))
        .collect()
}

pub(crate) fn request(session: &Rc<Session>, op: &str, id: &str, args: Value) -> Result<Value> {
    let a = args.as_array().ok_or_else(invalid)?;
    let arg = |index| a.get(index).cloned().unwrap_or_else(absent);
    if op == "free" {
        session.remove(id);
        return Ok(absent());
    }
    if id.is_empty() {
        return match op {
            "rpcIdentity" => {
                session
                    .host
                    .set_rpc_identity(rpc::RootOrigin::new(session.host.name(), text(&arg(0))?)?)?;
                Ok(absent())
            }
            "rpcAttach" => {
                let (endpoint, link) = crate::ipc::Endpoint::new();
                let peer = text(&arg(0))?.to_owned();
                let connection = if bool_arg(&arg(1))? {
                    session.host.accept_child(
                        link,
                        peer.clone(),
                        Value::Null,
                        rpc::ConnectionOptions::default(),
                    )?
                } else {
                    session.host.connect_parent(
                        link,
                        peer.clone(),
                        Value::Null,
                        rpc::ConnectionOptions::default(),
                    )?
                };
                session.host.set_route(peer, connection.clone())?;
                session.issue(Entry::Rpc(Rc::new(crate::ipc::RpcEndpoint {
                    endpoint,
                    connection,
                })))
            }
            "registerHook" => {
                crate::hooks::register(session, &arg(0))?;
                Ok(absent())
            }
            "registerOpaque" => {
                let id = text(&arg(0))?.to_owned();
                tembra_contracts::Contract::local(
                    "binding.opaque",
                    1,
                    json!({}),
                    std::collections::BTreeMap::from([(
                        "value".into(),
                        tembra_contracts::Member::Opaque { id: id.clone() },
                    )]),
                )
                .map_err(|e| RuntimeError::invalid(e.reason, e.message))?;
                if !session.opaque.borrow().contains(&id) && session.opaque.borrow().len() >= 1024 {
                    return Err(invalid());
                }
                crate::hooks::register_opaque(session, &id)?;
                Ok(absent())
            }
            "hold" => session.issue(Entry::Reference(session.host.hold(
                contract(&arg(0))?,
                parse(&arg(1))?,
                option_state(&arg(2))?,
            )?)),
            "install" => {
                let generation = crate::loader::Generation::new(session, "installed")?;
                let facet = facet(session, &arg(0), &arg(1), &arg(2), generation)?;
                let startup = session
                    .host
                    .install_executable(facet, LoadOptions::default())?;
                session.start(
                    async move {
                        startup.await?;
                        Ok(absent())
                    }
                    .boxed_local(),
                )
            }
            "beginPreparation" => session.issue(Entry::Preparation(session.loader.begin()?)),
            "prepareFacet" => {
                let Entry::Generation(generation) = session.entry(handle(&arg(3))?)? else {
                    return Err(invalid());
                };
                let facet = facet(session, &arg(0), &arg(1), &arg(2), generation)?;
                session.issue(Entry::Candidate(Rc::new(RefCell::new(Some(facet)))))
            }
            "prepareInstalled" => {
                let generation = crate::loader::Generation::new(session, "installed")?;
                let facet = facet(session, &arg(0), &arg(1), &arg(2), generation)?;
                session.issue(Entry::Candidate(Rc::new(RefCell::new(Some(facet)))))
            }
            "installPrepared" => {
                let mut facets = take_candidates(session, &json!([arg(0)]))?;
                let facet = facets.pop().ok_or_else(invalid)?;
                let startup = session
                    .host
                    .install_executable(facet, LoadOptions::default())?;
                session.start(
                    async move {
                        startup.await?;
                        Ok(absent())
                    }
                    .boxed_local(),
                )
            }
            "reload" => {
                let mut options = reload_options(&arg(1))?;
                let notification = if undefined(&arg(2)) {
                    None
                } else {
                    match session.entry(handle(&arg(2))?)? {
                        Entry::Signal(signal) => Some(signal.cancelled()),
                        _ => return Err(invalid()),
                    }
                };
                let cancellation = CancellationToken::new();
                options.cancellation = Some(cancellation.clone());
                let facets = take_candidates(session, &arg(0))?;
                let host = session.host.clone();
                session.start(
                    async move {
                        let transaction =
                            async move { Ok(report(host.reload(facets, options).await?)) }
                                .boxed_local();
                        if let Some(notification) = notification {
                            match futures::future::select(transaction, notification).await {
                                futures::future::Either::Left((result, _)) => result,
                                futures::future::Either::Right((reason, transaction)) => {
                                    cancellation.cancel(reason);
                                    transaction.await
                                }
                            }
                        } else {
                            transaction.await
                        }
                    }
                    .boxed_local(),
                )
            }
            "root" => {
                let callback = session.callback(&arg(0))?;
                let owner = session.clone();
                let host = session.host.clone();
                session.start(
                    async move {
                        if owner.closing.get() {
                            return Err(RuntimeError::aborted("host_closed", "Host is closing"));
                        }
                        host.root(move |ctx| run_root(owner, callback, ctx, None))
                            .await
                    }
                    .boxed_local(),
                )
            }
            "registry" => {
                session.issue(Entry::Registry(session.host.registry(&registry(&arg(0))?)?))
            }
            "cancel" => {
                context(session, &arg(0))?.context.cancel("requested");
                Ok(absent())
            }
            "observations" => {
                let limit = arg(0)
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(invalid)?;
                Ok(match session.host.observations() {
                    Some(observations) => {
                        json!({"events": observations.drain(limit), "statistics": observations.statistics()})
                    }
                    None => Value::Null,
                })
            }
            "diagnostics" => {
                let entries = session.entries.borrow();
                let mut kinds = std::collections::BTreeMap::<&str, usize>::new();
                for entry in entries.values() {
                    *kinds.entry(entry.kind()).or_default() += 1;
                }
                Ok(
                    json!({"codeVersion":session.host.code_version(), "loader":session.loader.diagnostics(),
                    "generations":session.generations.borrow().values().filter_map(std::rc::Weak::upgrade).map(|g|g.diagnostics()).collect::<Vec<_>>(),
                    "releasedGenerations":session.released_generations.borrow().clone(),
                    "jobs":session.jobs.get(), "callbackJobs":session.callback_jobs.get(), "handles":entries.len(), "handlesByKind":kinds, "errors":session.host.errors().into_iter().chain(session.callback_errors.borrow().iter().cloned()).collect::<Vec<_>>()}),
                )
            }
            "close" => {
                session.closing.set(true);
                let owner = session.clone();
                let contexts = session
                    .entries
                    .borrow()
                    .values()
                    .filter_map(|entry| match entry {
                        Entry::Context(ctx) => Some(ctx.context.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                for context in contexts {
                    context.cancel(RuntimeError::aborted("host_closed", "Host is closing"));
                }
                session.start(
                    async move {
                        let result = owner.host.close().await;
                        // Cancellation starts immediately; the dispatcher and loader leases remain
                        // valid until admitted JavaScript evaluation and managed callbacks finish.
                        while owner.callback_jobs.get() > 0
                            || owner
                                .entries
                                .borrow()
                                .values()
                                .any(|entry| matches!(entry, Entry::Preparation(_)))
                        {
                            owner.scheduler.driver.sleep(Duration::from_millis(1)).await;
                        }
                        let entries = std::mem::take(&mut *owner.entries.borrow_mut());
                        drop(entries);
                        owner.detach_dispatcher();
                        result?;
                        Ok(absent())
                    }
                    .boxed_local(),
                )
            }
            _ => Err(invalid()),
        };
    }
    let entry = session.entry(id)?;
    match (entry, op) {
        (Entry::Rpc(rpc), "drain") => Ok(json!(rpc.endpoint.drain())),
        (Entry::Rpc(rpc), "receive") => {rpc.endpoint.receive(text(&arg(0))?.to_owned())?;Ok(absent())},
        (Entry::Rpc(rpc), "ready") => session.start(async move {rpc.connection.ready().await?;Ok(absent())}.boxed_local()),
        (Entry::Rpc(rpc), "closed") => session.start(async move {Ok(json!(rpc.connection.closed().await.error))}.boxed_local()),
        (Entry::Rpc(rpc), "status") => Ok(json!(match rpc.connection.status(){rpc::ConnectionStatus::AwaitHello=>"connecting",rpc::ConnectionStatus::Open=>"open",rpc::ConnectionStatus::Closed=>"closed"})),
        (Entry::Rpc(rpc), "close") => {rpc.connection.close(rpc::CloseReason::closed());Ok(absent())},
        (Entry::Preparation(preparation), operation @ ("reserve" | "reserveSources")) => {
            let costs = arg(0).as_array().ok_or_else(invalid)?.iter()
                .map(|cost| cost.as_u64().and_then(|n|usize::try_from(n).ok()).ok_or_else(invalid))
                .collect::<Result<Vec<_>>>()?;
            if operation=="reserveSources" {preparation.reserve_sources(costs,session.host.code_version())?;} else {preparation.reserve(costs, session.host.code_version())?;}
            Ok(absent())
        }
        (Entry::Preparation(preparation), operation @ ("submit" | "submitSource")) => {
            let source_hash = if undefined(&arg(0)) { None } else {
                let hash = text(&arg(0))?.to_owned();
                if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) {
                    return Err(invalid());
                }
                Some(hash)
            };
            let generation = crate::loader::Generation::new(session, if operation=="submitSource" {"source"}else{"esm"})?;
            *generation.source_hash.borrow_mut() = source_hash;
            let warning = if operation=="submitSource" {preparation.submit_source(session.host.code_version())?} else {preparation.submit()?};
            Ok(json!({"generation":session.issue(Entry::Generation(generation.clone()))?,
                "identity":generation.id,"warning":warning}))
        }
        (Entry::Preparation(preparation), "chargeSourceModule") => {
            let bytes=arg(0).as_u64().and_then(|n|usize::try_from(n).ok()).ok_or_else(invalid)?;
            preparation.charge_source_module(bytes,session.host.code_version())?;
            Ok(absent())
        }
        (Entry::Preparation(preparation), "evaluationFailed") => {
            preparation.failed(arg(0).as_bool().unwrap_or(false));
            Ok(absent())
        }
        (Entry::Preparation(preparation), "committing") => {
            preparation.stage.set("committing");
            Ok(absent())
        }
        (Entry::Sink(delivery), "emit") => {
            let delivery = delivery.upgrade().ok_or_else(|| RuntimeError::aborted("expired", "event subscription ended"))?;
            (delivery.listener)(Rc::new(NodeValue::new(session, &arg(0))?));
            Ok(absent())
        },
        (Entry::Reference(reference), "statusJson") => Ok(json!(json!(status(reference.status())).to_string())),
        (Entry::Reference(reference), "shapeJson") => Ok(json!(reference.shape().map_or_else(|| json!({"present":false}), |v| json!({"present":true,"value":shape(v)})).to_string())),
        (Entry::Reference(reference), "errorJson") => Ok(json!(reference.error().map_or_else(|| json!({"present":false}), |v| json!({"present":true,"value":v})).to_string())),
        (Entry::Reference(reference), "release") => { reference.release(); Ok(absent()) },
        (Entry::Reference(reference), "ready") => session.start(async move { reference.ready().await?; Ok(absent()) }.boxed_local()),
        (Entry::Reference(reference), "state") => session.issue(Entry::State(reference.readable(text(&arg(0))?)?)),
        (Entry::Reference(reference), "call") => {
            if bool_arg(&arg(3))? { return Err(RuntimeError::invalid("native_adapter", "native signatures require an explicit whole-signature adapter")); }
            let member = text(&arg(0))?.to_owned();
            let input = payload(parse(&arg(1))?)?;
            let caller = if undefined(&arg(2)) { None } else { Some(context(session, &arg(2))?) };
            let physical = caller.as_ref().and_then(|ctx| ctx.invocation.as_ref().map(|owner| owner.retain(&ctx.context))).transpose()?;
            session.start(async move {
                let _physical = physical;
                let value = reference.call_json(&member, input, caller.as_ref().map(|c| &c.context)).await?;
                Ok(json!(envelope(value).to_string()))
            }.boxed_local())
        },
        (Entry::Reference(reference), "onStatus") => {
            let callback = session.callback(&arg(0))?;
            removal(session, reference.on_status(move |status, error| { callback.notify(vec![json!(json!({"status":status_name(status), "error":error}).to_string())]); }))
        },
        (Entry::Reference(reference), "onShape") => {
            let callback = session.callback(&arg(0))?;
            removal(session, reference.on_shape(move |shape| { callback.notify(vec![json!(self::shape(shape).to_string())]); }))
        },
        (Entry::State(state), "snapshotJson") => Ok(json!(snapshot(&state.snapshot()?).to_string())),
        (Entry::State(state), "snapshotInJson") => Ok(json!(snapshot(&state.snapshot_in(&context(session, &arg(0))?.context)?).to_string())),
        (Entry::State(state), "subscribe") => {
            let callback = session.callback(&arg(0))?;
            let subscription = state.subscribe(move |value| callback.sync(vec![json!(snapshot(&value).to_string())]).map(|_| ()))?;
            removal(session, Box::new(move || subscription.unsubscribe()))
        },
        (Entry::Cell(cell), "state") => session.issue(Entry::State(cell.readable())),
        (Entry::Cell(cell), "replace") => {
            let context = context(session, &arg(0))?;
            let result = cell.replace(&context.context, parse(&arg(1))?, ReplaceOptions { diff: bool_arg(&arg(2))? })?;
            Ok(json!(snapshot(&result).to_string()))
        },
        (Entry::Cell(cell), "change") => {
            let context = context(session, &arg(0))?;
            let callback = session.callback(&arg(1))?;
            let result = cell.change(&context.context, |value| { *value = parse(&callback.sync(vec![json!(value.to_string())])?)?; Ok(()) })?;
            Ok(json!(snapshot(&result).to_string()))
        },
        (Entry::Subscription(subscription), "unsubscribe") => { subscription.remove(); Ok(absent()) },
        (Entry::Context(_), "token") => Ok(json!({"$handle":id, "$kind":"context"})),
        (Entry::Context(ctx), "check") => { ctx.context.check(session.host.id())?; Ok(absent()) },
        (Entry::Context(ctx), "cancel") => { ctx.context.check(session.host.id())?; ctx.context.cancel(text(&arg(0))?); Ok(absent()) },
        (Entry::Context(ctx), "principalJson") => Ok(json!(ctx.context.principal().to_string())),
        (Entry::Context(ctx), "codeVersion") => Ok(json!(ctx.context.code_version())),
        (Entry::Context(ctx), "observationHostId") => Ok(json!(ctx.context.host_id().to_string())),
        (Entry::Context(ctx), "observationId") => Ok(json!(ctx.context.observation_id().map(|id| id.to_string()))),
        (Entry::Context(ctx), "signal") => core_signal(session, Signal::Token(ctx.signal)),
        (Entry::Context(ctx), "get") => {
            ctx.context.check(session.host.id())?;
            let contract = contract(&arg(0))?;
            let input = parse(&arg(1))?;
            let state = bool_arg(&arg(2))?;
            let physical = ctx.invocation.as_ref().map(|owner| owner.retain(&ctx.context)).transpose()?;
            let owner = session.clone();
            session.start(async move {
                let _physical = physical;
                owner.issue(Entry::Reference(ctx.context.get(contract, input, GetOptions { state, ..GetOptions::default() }).await?))
            }.boxed_local())
        },
        (Entry::Signal(signal), "aborted") => Ok(json!(signal.aborted())),
        (Entry::Signal(signal), "reasonJson") => Ok(json!(if signal.aborted() { signal.cancelled().now_or_never().map_or(Value::Null, |e| json!(e)) } else { Value::Null }.to_string())),
        (Entry::Signal(signal), "cancelled") => session.start(async move { signal.cancelled().await; Ok(absent()) }.boxed_local()),
        (Entry::Scope(scope), "runHook"|"runHookThen") => crate::hooks::run(session,scope,id,&arg(0),&arg(1),&arg(2),&arg(3)),
        (Entry::Continuation(next),operation @ ("call"|"recover")) => {
            let input=session.callback(&arg(0))?;
            if session.jobs.get()>=128 {return Err(RuntimeError::unavailable("jobs_limit","callable ingress capacity exhausted").with_details(json!({"callablePhase":"ingress"})));}
            let (future,mut completion)=next.prepare(input,operation=="recover").into_parts();
            completion.admit()?;
            session.start(async move {
                let result=future.await;
                completion.complete(Ok(()));
                result
            }.boxed_local())
        },
        (Entry::Reference(reference), "opaque") => {
            let member=text(&arg(0))?.to_owned();
            let nominal=match reference.contract().members("").and_then(|m|m.get(&member)) {Some(tembra_contracts::Member::Opaque{id})=>id.clone(),_=>return Err(invalid())};
            if !session.opaque.borrow().contains(&nominal) {return Err(RuntimeError::invalid("opaque","nominal is not registered"));}
            Ok(reference.opaque::<NodeValue>(&member)?.wire())
        },
        (Entry::Resource(resource), "capability") => {
            let nominal=text(&arg(0))?.to_owned();
            if !session.opaque.borrow().contains(&nominal) {return Err(RuntimeError::invalid("opaque","nominal is not registered"));}
            resource.upgrade()?;
            session.issue(Entry::Capability{resource,nominal})
        },
        (Entry::Scope(scope), "hostName") => Ok(json!(scope.host_name())),
        (Entry::Scope(scope), "signal") => core_signal(session, Signal::Facet(scope.signal())),
        (Entry::Scope(scope), "registry") => session.issue(Entry::Registry(scope.registry(&registry(&arg(0))?)?)),
        (Entry::Scope(scope), "root") => {
            let callback = session.callback(&arg(0))?;
            let owner = session.clone();
            let result = scope.root(move |ctx| run_root(owner, callback, ctx, None));
            session.start(result)
        },
        (Entry::Task(scope), "signal") => core_signal(session, Signal::Facet(scope.signal())),
        (Entry::Task(scope), "root") => {
            let callback = session.callback(&arg(0))?;
            let owner = session.clone();
            let lease = scope.retain()?;
            let result = scope.root(move |ctx| run_root(owner, callback, ctx, Some(lease)));
            session.start(result)
        },
        (Entry::Recorder(recorder), "serve") => {
            let token = contract(&arg(0))?;
            let callback = session.callback(&arg(1))?;
            let options = serve_options(session, &token, &arg(2), &arg(3))?;
            recorder.serve(token.clone(), factory(session, callback, token), options)?;
            Ok(absent())
        },
        (Entry::Object(weak), operation) => {
            let owner = weak.upgrade().ok_or_else(invalid)?;
            object_request(session, owner, operation, a)
        },
        (Entry::ResourceBuilder(builder), "referenceDependency") => { builder.borrow_mut().as_mut().ok_or_else(invalid)?.deps.push(reference(session, &arg(0))?.dependency()); Ok(absent()) },
        (Entry::ResourceBuilder(builder), "resourceDependency") => { builder.borrow_mut().as_mut().ok_or_else(invalid)?.deps.push(resource(session, &arg(0))?.dependency()); Ok(absent()) },
        (Entry::ResourceBuilder(builder), "finish") => {
            let builder = builder.borrow_mut().take().ok_or_else(invalid)?;
            let owner = builder.owner.upgrade().ok_or_else(invalid)?;
            let weak = Rc::downgrade(session);
            let opener = builder.open;
            let mut declaration = ResourceDeclaration::new(move |signal| {
                let weak = weak.clone(); let opener = opener.clone();
                async move {
                    let session = weak.upgrade().ok_or_else(invalid)?;
                    let signal = core_signal(&session, Signal::Token(signal))?;
                    let value = opener.invoke(vec![signal], true).await?;
                    NodeValue::new(&session, &value)
                }.boxed_local()
            });
            declaration.version = builder.version; declaration.exclusive = builder.exclusive; declaration.deps = builder.deps;
            if let Some(closer) = builder.close { declaration = declaration.with_close(move |value| { let closer = closer.clone(); async move { closer.invoke_void(vec![value.wire()]).await?; Ok(()) }.boxed_local() }); }
            let value = owner.recorder.resource(text(&arg(0))?, declaration)?;
            session.issue(Entry::Resource(value.downgrade()))
        },
        (Entry::Resource(value), "value") => Ok(value.upgrade()?.value()?.wire()),
        (Entry::Resource(value), "version") => Ok(json!(value.upgrade()?.version())),
        (Entry::Resource(value), "exclusive") => Ok(json!(value.upgrade()?.exclusive())),
        (Entry::Derivation(builder), "source") => { builder.borrow_mut().as_mut().ok_or_else(invalid)?.sources.push(source(session, &arg(0))?); Ok(absent()) },
        (Entry::Derivation(builder), "finish") => {
            let builder = builder.borrow_mut().take().ok_or_else(invalid)?;
            let owner = builder.owner.upgrade().ok_or_else(invalid)?;
            let callback = builder.compute;
            let value = owner.recorder.derive(builder.sources, builder.schema, move |values| {
                parse(&callback.sync(vec![json!(Value::Array(values.iter().map(snapshot).collect()).to_string())])?)
            })?;
            session.issue(Entry::State(value))
        },
        (Entry::Registry(view), "table") => {
            let ctx = if undefined(&arg(0)) { None } else { Some(context(session, &arg(0))?.context) };
            session.issue(Entry::Table(view.table(ctx.as_ref())?))
        },
        (Entry::Registry(view), "subscribe") => {
            let callback = session.callback(&arg(0))?;
            let weak = Rc::downgrade(session);
            let subscription = view.subscribe(move |table| { let owner = weak.upgrade().ok_or_else(invalid)?; callback.sync(vec![owner.issue(Entry::Table(table))?]).map(|_| ()) })?;
            removal(session, Box::new(move || subscription.unsubscribe()))
        },
        (Entry::Table(table), "entriesJson") => Ok(json!(json!({"codeVersion":table.code_version(), "entries":table.entries().map(|(key,entry)| json!({"key":key,"data":entry.data(),"operation":entry.operation().is_some()})).collect::<Vec<_>>()} ).to_string())),
        (Entry::Table(table), "operation") => match table.get(text(&arg(0))?).and_then(RegistryEntry::operation) { Some(op) => session.issue(Entry::Operation(op.clone())), None => Ok(absent()) },
        (Entry::Operation(operation), "metadataJson") => {
            let member = operation.contract().variants().values().find_map(|members| members.get(operation.method())).ok_or_else(invalid)?;
            Ok(json!(serde_json::to_string(member).map_err(|_| invalid())?))
        },
        (Entry::Operation(operation), "call") => {
            if bool_arg(&arg(2))? { return Err(RuntimeError::invalid("native_adapter", "native registry signatures need an explicit adapter")); }
            let ctx = context(session, &arg(0))?;
            let physical = ctx.invocation.as_ref().map(|owner| owner.retain(&ctx.context)).transpose()?;
            let input = payload(parse(&arg(1))?)?;
            session.start(async move { let _physical = physical; Ok(json!(envelope(operation.call_json(&ctx.context, input).await?).to_string())) }.boxed_local())
        },
        _ => Err(invalid()),
    }
}

fn run_root(
    session: Rc<Session>,
    callback: Rc<crate::callbacks::Callback>,
    context: Context,
    physical: Option<InvocationLease>,
) -> LocalBoxFuture<'static, Result<Value>> {
    async move {
        let _physical = physical;
        let lease = ContextLease::issue(&session, context, None)?;
        let value = callback.invoke(vec![lease.wire.clone()], true).await?;
        Ok(NodeValue::new(&session, &value)?.transfer())
    }
    .boxed_local()
}

fn object_request(
    session: &Rc<Session>,
    owner: Rc<ObjectOwner>,
    op: &str,
    a: &[Value],
) -> Result<Value> {
    let arg = |index| a.get(index).cloned().unwrap_or_else(absent);
    if op == "exposeOpaque" {
        let name = text(&arg(0))?.to_owned();
        let Entry::Capability { resource, nominal } = session.entry(handle(&arg(1))?)? else {
            return Err(invalid());
        };
        if owner
            .opaque
            .borrow_mut()
            .insert(name, (resource, nominal))
            .is_some()
        {
            return Err(invalid());
        }
        return Ok(absent());
    }
    if op == "exposeState" {
        let name = text(&arg(0))?.to_owned();
        let state = source(session, &arg(1))?;
        if owner.states.borrow_mut().insert(name, state).is_some() {
            return Err(RuntimeError::invalid(
                "duplicate",
                "duplicate state exposure",
            ));
        }
        return Ok(absent());
    }
    owner.recorder.invocation_owner()?;
    match op {
        "get" => session.issue(Entry::Reference(owner.recorder.get(
            contract(&arg(0))?,
            parse(&arg(1))?,
            GetOptions {
                state: bool_arg(&arg(2))?,
                ..GetOptions::default()
            },
        )?)),
        "cell" => {
            let schema = parse(&arg(1))?;
            let mut declaration = if undefined(&arg(3)) {
                if undefined(&arg(2)) {
                    return Err(RuntimeError::invalid(
                        "state",
                        "cell requires initial or initialize",
                    ));
                }
                StateDeclaration::new(schema, parse(&arg(2))?)
            } else {
                if !undefined(&arg(2)) {
                    return Err(RuntimeError::invalid(
                        "state",
                        "cell cannot have both initial and initialize",
                    ));
                }
                let callback = session.callback(&arg(3))?;
                let weak = Rc::downgrade(session);
                let initializer_owner = owner.clone();
                StateDeclaration::with_initializer(schema, move |context| {
                    let callback = callback.clone();
                    let weak = weak.clone();
                    let initializer_owner = initializer_owner.clone();
                    async move {
                        // Core activation owns this initializer; it is not an entered receiver call.
                        let _owner = initializer_owner;
                        let session = weak.upgrade().ok_or_else(invalid)?;
                        let ctx = ContextLease::issue(&session, context, None)?;
                        parse(&callback.invoke(vec![ctx.wire.clone()], false).await?)
                    }
                    .boxed_local()
                })
            }
            .version(number(&arg(4))?);
            if !undefined(&arg(5)) {
                let migrate = session.callback(&arg(5))?;
                declaration = declaration.with_migrate(move |value, version| {
                    parse(&migrate.sync(vec![json!(value.to_string()), json!(version)])?)
                });
            }
            session.issue(Entry::Cell(
                owner.recorder.state(text(&arg(0))?, declaration)?,
            ))
        }
        "beginResource" => {
            let open = session.callback(&arg(0))?;
            let close = if undefined(&arg(1)) {
                None
            } else {
                Some(session.callback(&arg(1))?)
            };
            let flags = parse(&arg(2))?;
            if flags
                .as_object()
                .is_none_or(|f| f.keys().any(|k| k != "version" && k != "exclusive"))
            {
                return Err(invalid());
            }
            session.issue(Entry::ResourceBuilder(Rc::new(RefCell::new(Some(
                ResourceBuilder {
                    owner: Rc::downgrade(&owner),
                    open,
                    close,
                    version: number(&flags["version"])?,
                    exclusive: bool_arg(&flags["exclusive"])?,
                    deps: Vec::new(),
                },
            )))))
        }
        "derive" => session.issue(Entry::Derivation(Rc::new(RefCell::new(Some(Derivation {
            owner: Rc::downgrade(&owner),
            schema: parse(&arg(0))?,
            compute: session.callback(&arg(1))?,
            sources: Vec::new(),
        }))))),
        "register" => {
            let registry = registry(&arg(0))?;
            if undefined(&arg(3)) {
                owner
                    .recorder
                    .register(&registry, text(&arg(1))?, parse(&arg(2))?)?;
            } else {
                owner.recorder.register_method(
                    &registry,
                    text(&arg(1))?,
                    parse(&arg(2))?,
                    text(&arg(3))?,
                )?;
            }
            Ok(absent())
        }
        "onState" => {
            let callback = session.callback(&arg(2))?;
            let weak = Rc::downgrade(session);
            let invocation = owner.invocation.clone();
            owner.recorder.on_with_options(
                text(&arg(0))?,
                source(session, &arg(1))?,
                move |value, context| {
                    deliver(
                        weak.clone(),
                        callback.clone(),
                        invocation.clone(),
                        context,
                        snapshot(&value),
                    )
                },
                listener_options(&arg(3))?,
            )?;
            Ok(absent())
        }
        "onEvent" => {
            let binding = crate::activity::event_source(session, &arg(1))?;
            let callback = session.callback(&arg(2))?;
            let weak = Rc::downgrade(session);
            let invocation = owner.invocation.clone();
            owner.recorder.on_event_with_options(
                text(&arg(0))?,
                binding.source.clone().retain_owner(binding),
                move |value, context| {
                    let value = value.clone();
                    let result = deliver(
                        weak.clone(),
                        callback.clone(),
                        invocation.clone(),
                        context,
                        value.wire(),
                    );
                    async move {
                        let _value = value;
                        result.await
                    }
                    .boxed_local()
                },
                listener_options(&arg(3))?,
            )?;
            Ok(absent())
        }
        "onResource" => {
            let select = session.callback(&arg(2))?;
            let callback = session.callback(&arg(3))?;
            let weak = Rc::downgrade(session);
            let selector_session = Rc::downgrade(session);
            let invocation = owner.invocation.clone();
            owner.recorder.on_resource_with_options(
                text(&arg(0))?,
                resource(session, &arg(1))?,
                Rc::new(move |value: Rc<NodeValue>| {
                    let session = selector_session.upgrade().ok_or_else(invalid)?;
                    let selected = select.sync_retained(vec![value.wire()])?;
                    let binding = crate::activity::event_source(&session, &selected)?;
                    Ok(binding.source.clone().retain_owner(binding))
                }),
                move |value, context| {
                    let result = deliver(
                        weak.clone(),
                        callback.clone(),
                        invocation.clone(),
                        context,
                        value.wire(),
                    );
                    async move {
                        let _value = value;
                        result.await
                    }
                    .boxed_local()
                },
                listener_options(&arg(4))?,
            )?;
            Ok(absent())
        }
        "task" => {
            let callback = session.callback(&arg(0))?;
            let weak = Rc::downgrade(session);
            owner.recorder.task(move |scope| {
                let callback = callback.clone();
                let weak = weak.clone();
                async move {
                    let session = weak.upgrade().ok_or_else(invalid)?;
                    callback
                        .invoke_void(vec![session.issue(Entry::Task(scope))?])
                        .await?;
                    Ok(())
                }
                .boxed_local()
            })?;
            Ok(absent())
        }
        _ => Err(invalid()),
    }
}
fn listener_options(value: &Value) -> Result<ListenerOptions> {
    let value = parse(value)?;
    let fields = value.as_object().ok_or_else(invalid)?;
    if fields.keys().any(|k| k != "maxPending") {
        return Err(invalid());
    }
    match fields.get("maxPending") {
        None => Ok(ListenerOptions::default()),
        Some(value) => {
            ListenerOptions::new(usize::try_from(number(value)?).map_err(|_| invalid())?)
        }
    }
}
fn deliver(
    session: std::rc::Weak<Session>,
    callback: Rc<crate::callbacks::Callback>,
    invocation: InvocationOwner,
    context: Context,
    value: Value,
) -> LocalBoxFuture<'static, Result<()>> {
    async move {
        let session = session.upgrade().ok_or_else(invalid)?;
        let _physical = invocation.retain(&context)?;
        let lease = ContextLease::issue(&session, context, Some(invocation))?;
        callback
            .invoke_void(vec![lease.wire.clone(), value])
            .await?;
        Ok(())
    }
    .boxed_local()
}

fn status_name(value: RefStatus) -> &'static str {
    match value {
        RefStatus::Loading => "loading",
        RefStatus::Live => "live",
        RefStatus::Released => "released",
        RefStatus::Gone => "gone",
    }
}
fn status(value: RefStatus) -> &'static str {
    status_name(value)
}
fn shape(value: Shape) -> Value {
    json!({"variant":value.variant, "members":value.members, "contract":value.contract})
}
