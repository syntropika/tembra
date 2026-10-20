pub(crate) mod pump;
#[cfg(test)]
mod tests;
use crate::{
    artifact::BundledJsArtifact,
    binding,
    options::{ExecutorOptions, ExecutorStats},
    slots::{Callback, JsValue, Module, Owner},
};
use deno_core::{JsRuntime, ModuleSpecifier, RuntimeOptions, v8};
use futures::{
    FutureExt, Stream,
    channel::{mpsc, oneshot},
    future::{LocalBoxFuture, poll_fn},
    stream::{FusedStream, FuturesUnordered},
};
use pump::pump;
use serde_json::Value;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    pin::Pin,
    rc::{Rc, Weak},
    task::Poll,
};
use tembra_core::{Driver, ExecutableFacet, InvocationLease, Result, RuntimeError};

/// Local execution adapter. It never owns a strong Host.
#[derive(Clone)]
pub struct Executor {
    pub(crate) inner: Rc<Engine>,
}
pub(crate) struct Engine {
    pub(crate) id: u64,
    pub(crate) driver: Rc<dyn Driver>,
    pub(crate) options: ExecutorOptions,
    pub(crate) capabilities: RefCell<BTreeMap<u64, Weak<crate::slots::Capability>>>,
    pub(crate) next_capability: Cell<u64>,
    pub(crate) counters: Cell<ExecutorStats>,
    errors: RefCell<VecDeque<RuntimeError>>,
    sender: mpsc::UnboundedSender<Request>,
    pub(crate) pump_waker: futures::task::AtomicWaker,
    lexer: v8::Global<v8::Function>,
    pub(crate) bindings: v8::Global<v8::Object>,
    pub(crate) brand: v8::Global<v8::Private>,
    pub(crate) natives: RefCell<
        BTreeMap<crate::native::NativeMethodSignature, Rc<dyn crate::native::ErasedNative>>,
    >,
    pub(crate) hooks: RefCell<BTreeMap<String, tembra_hooks::portable::PortableHook>>,
    pub(crate) activities: binding::activity::Cache,
    pub(crate) opaques: RefCell<BTreeMap<String, Rc<dyn crate::native::ErasedOpaque>>>,
    // All isolate-owned Globals above are dropped before the runtime.
    pub(crate) runtime: RefCell<JsRuntime>,
}
thread_local! {
    static NEXT_ENGINE: Cell<u64> = const { Cell::new(1) };
    static THREAD_ENGINE: RefCell<Weak<Engine>> = const { RefCell::new(Weak::new()) };
}
pub(crate) struct CallableJob {
    pub resolver: v8::Global<v8::PromiseResolver>,
    pub module: Rc<Module>,
    pub owner: Rc<Owner>,
    pub lease: InvocationLease,
    pub call: crate::native::NativeCall,
    pub refusal: Option<RuntimeError>,
}
pub(crate) enum Request {
    Callable(CallableJob),
    Resolve {
        resolver: v8::Global<v8::PromiseResolver>,
        module: Rc<Module>,
        result: Result<crate::binding::asynchronous::NativeOutput>,
        owner: Option<Rc<Owner>>,
        lease: Option<InvocationLease>,
        completion: Option<crate::native::CompletionGuard>,
    },
    Prepare {
        host: u64,
        artifact: BundledJsArtifact,
        export: String,
        reply: oneshot::Sender<Result<ExecutableFacet>>,
    },
    Invoke(Invocation),
}
pub(crate) enum Output {
    Owned(JsValue),
    Portable(Option<Value>),
    Native(Box<dyn std::any::Any>),
}
pub(crate) enum Conversion {
    Owned,
    Portable(bool),
    NativeVoid,
    Native(Box<dyn FnOnce(crate::native::NativeValue) -> Result<Box<dyn std::any::Any>>>),
}
pub(crate) struct Invocation {
    callback: Rc<Callback>,
    args: InvocationArguments,
    owner: Option<Rc<Owner>>,
    lease: Option<InvocationLease>,
    conversion: Conversion,
    reply: oneshot::Sender<Result<Output>>,
}
// Only the registered typed core closer can construct ResourceClose. Ordinary
// invocation arguments retain exact executable-generation checks.
enum InvocationArguments {
    Ordinary(Vec<JsValue>),
    ResourceClose(Rc<JsValue>),
    Event { context: JsValue, value: JsValue },
}
impl InvocationArguments {
    fn values(&self) -> &[JsValue] {
        match self {
            Self::Ordinary(values) => values,
            Self::ResourceClose(value) => std::slice::from_ref(value.as_ref()),
            Self::Event { .. } => &[],
        }
    }
    fn compatible(&self, engine: &Engine, callback: &Callback) -> bool {
        if let Self::Event { context, value } = self {
            return context.module.engine.id == engine.id
                && value.module.engine.id == engine.id
                && context.module.host == callback.module.host
                && value.module.host == callback.module.host
                && context.module.generation == callback.module.generation;
        }
        self.values().iter().all(|value| {
            value.module.engine.id == engine.id
                && value.module.host == callback.module.host
                && (matches!(self, Self::ResourceClose(_))
                    || value.module.generation == callback.module.generation)
        })
    }
}
impl InvocationArguments {
    fn globals(&self) -> Vec<v8::Global<v8::Value>> {
        match self {
            Self::Event { context, value } => vec![context.value.clone(), value.value.clone()],
            _ => self
                .values()
                .iter()
                .map(|value| value.value.clone())
                .collect(),
        }
    }
}
type Evaluation = LocalBoxFuture<'static, std::result::Result<(), deno_core::error::CoreError>>;

impl Executor {
    /// Creates one engine and starts its Driver-owned local pump.
    ///
    /// # Errors
    /// Refuses zero limits, a missing entered Tokio runtime (`unavailable/tokio_runtime`),
    /// another live engine on this thread (`unavailable/engine_thread`), or bootstrap failure.
    /// Hosts on one thread can share this executor. Dropping an executor does not end an
    /// engine still retained by installed modules or physical work. V8 platform
    /// initialization can panic for unsupported embedding/platform configurations.
    pub fn new(driver: Rc<dyn Driver>, options: ExecutorOptions) -> Result<Self> {
        options.check()?;
        tokio::runtime::Handle::try_current().map_err(|_| {
            RuntimeError::unavailable(
                "tokio_runtime",
                "the native JavaScript executor requires an entered Tokio runtime",
            )
        })?;
        if THREAD_ENGINE.with(|slot| slot.borrow().upgrade().is_some()) {
            return Err(RuntimeError::unavailable(
                "engine_thread",
                "this thread already owns a live JavaScript engine; share its executor",
            ));
        }
        let id = NEXT_ENGINE.with(|next| {
            let id = next.get();
            next.set(id.checked_add(1).expect("engine identity exhausted"));
            id
        });
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: extensions(),
            ..Default::default()
        });
        let lexer_source = include_str!("../vendor/es-module-lexer/lexer.minimal.asm.js")
            .replace("export const init", "const init")
            .replace("export function parse", "function parse");
        // The lexical parser has its own primordials in the same isolate. Application
        // prototype changes cannot change admission of later executable artifacts.
        let lexer = private_lexer(&mut runtime, &lexer_source)?;
        let bootstrap = runtime
            .execute_script("tembra:bindings", include_str!("binding/bootstrap.js"))
            .map_err(js_error)?;
        #[cfg(test)]
        runtime
            .execute_script(
                "tembra:test-gate",
                "{const ops=Deno.core.ops;globalThis.__tembraTestGate=()=>ops.op_test_gate();}",
            )
            .map_err(js_error)?;
        runtime
            .execute_script(
                "tembra:ambient",
                r#"
                    if (!Reflect.deleteProperty(globalThis, 'Deno')) {
                        throw Error('internal runtime globals could not be hidden');
                    }
                "#,
            )
            .map_err(js_error)?;
        let (lexer, bindings, brand) = {
            deno_core::scope!(scope, runtime);
            let lexer = v8::Local::new(scope, lexer);
            let bindings = v8::Local::new(scope, bootstrap);
            let lexer = v8::Local::<v8::Function>::try_from(lexer)
                .map_err(|_| invalid("bootstrap", "lexer export is not callable"))?;
            let bindings = v8::Local::<v8::Object>::try_from(bindings)
                .map_err(|_| invalid("bootstrap", "binding bootstrap is not an object"))?;
            let brand = v8::Private::new(scope, None);
            (
                v8::Global::new(scope, lexer),
                v8::Global::new(scope, bindings),
                v8::Global::new(scope, brand),
            )
        };
        let (sender, receiver) = mpsc::unbounded();
        let inner = Rc::new(Engine {
            id,
            driver: driver.clone(),
            options,
            runtime: RefCell::new(runtime),
            capabilities: RefCell::default(),
            next_capability: Cell::new(1),
            counters: Cell::default(),
            pump_waker: futures::task::AtomicWaker::new(),
            errors: RefCell::default(),
            sender,
            lexer,
            bindings,
            brand,
            natives: RefCell::default(),
            opaques: RefCell::default(),
            activities: binding::activity::Cache::default(),
            hooks: RefCell::default(),
        });
        THREAD_ENGINE.with(|slot| *slot.borrow_mut() = Rc::downgrade(&inner));
        inner
            .runtime
            .borrow()
            .op_state()
            .borrow_mut()
            .put(Rc::downgrade(&inner));
        driver.spawn(pump(Rc::downgrade(&inner), receiver).boxed_local());
        Ok(Self { inner })
    }
    /// Validates and evaluates one selected export without installing it.
    ///
    /// The declared export set must exactly match the lexical module exports. Evaluation
    /// attempts consume cumulative module/source budgets even on later evaluation failure.
    /// Dropping the waiter does not undo evaluation already admitted to the pump.
    ///
    /// # Errors
    /// Refuses artifact/export/profile/queue budgets, JavaScript syntax/evaluation failures,
    /// and invalid facet definitions. No Host is acquired or mutated by preparation.
    pub fn prepare(
        &self,
        host_id: u64,
        artifact: BundledJsArtifact,
        export_name: String,
    ) -> LocalBoxFuture<'static, Result<ExecutableFacet>> {
        let result = (|| {
            if artifact.source.len() > self.inner.options.max_source_bytes {
                return Err(limit("source_bytes"));
            }
            self.inner.admit_queue()?;
            let (reply, receive) = oneshot::channel();
            if self
                .inner
                .sender
                .unbounded_send(Request::Prepare {
                    host: host_id,
                    artifact,
                    export: export_name,
                    reply,
                })
                .is_err()
            {
                self.inner.dequeued();
                return Err(closed());
            }
            Ok(receive)
        })();
        async move { result?.await.map_err(|_| closed())? }.boxed_local()
    }
    /// Drains up to the latest 128 asynchronous event-loop or observer diagnostics.
    /// These diagnostics do not imply physical promise termination. The engine continues
    /// observing each admitted promise; older diagnostics are evicted when the buffer fills.
    pub fn take_errors(&self) -> Vec<RuntimeError> {
        self.inner.errors.borrow_mut().drain(..).collect()
    }
    /// Observes retained engine admission without changing it.
    pub fn stats(&self) -> ExecutorStats {
        let mut stats = self.inner.counters.get();
        stats.capabilities = self
            .inner
            .capabilities
            .borrow()
            .values()
            .filter(|cap| cap.strong_count() > 0)
            .count();
        stats
    }
}
impl Engine {
    pub(crate) fn report(&self, error: RuntimeError) {
        let evicted = {
            let mut errors = self.errors.borrow_mut();
            let old = if errors.len() == 128 {
                errors.pop_front()
            } else {
                None
            };
            errors.push_back(error);
            old
        };
        drop(evicted);
    }
    pub(crate) fn admit_queue(&self) -> Result<()> {
        let mut counters = self.counters.get();
        if counters.queued_jobs >= self.options.max_queued_jobs {
            return Err(limit("queued_jobs"));
        }
        counters.queued_jobs += 1;
        self.counters.set(counters);
        Ok(())
    }
    pub(crate) fn dequeued(&self) {
        let mut counters = self.counters.get();
        counters.queued_jobs -= 1;
        self.counters.set(counters);
    }
    pub(crate) fn update(&self, change: impl FnOnce(&mut ExecutorStats)) {
        let mut stats = self.counters.get();
        change(&mut stats);
        self.counters.set(stats);
    }
    pub(crate) fn enqueue(
        self: &Rc<Self>,
        callback: Rc<Callback>,
        args: Vec<JsValue>,
        owner: Option<Rc<Owner>>,
        lease: Option<InvocationLease>,
        conversion: Conversion,
    ) -> LocalBoxFuture<'static, Result<Output>> {
        self.enqueue_arguments(
            callback,
            InvocationArguments::Ordinary(args),
            owner,
            lease,
            conversion,
        )
    }
    /// Only used by the actual typed ResourceDeclaration closer. The value keeps its
    /// original module; the new callback separately owns its own module until settlement.
    pub(crate) fn close_resource(
        self: &Rc<Self>,
        callback: Rc<Callback>,
        value: Rc<JsValue>,
    ) -> LocalBoxFuture<'static, Result<Output>> {
        self.enqueue_arguments(
            callback,
            InvocationArguments::ResourceClose(value),
            None,
            None,
            Conversion::Portable(false),
        )
    }
    pub(crate) fn deliver_event(
        self: &Rc<Self>,
        callback: Rc<Callback>,
        context: JsValue,
        value: JsValue,
        owner: Rc<Owner>,
        lease: InvocationLease,
    ) -> LocalBoxFuture<'static, Result<Output>> {
        self.enqueue_arguments(
            callback,
            InvocationArguments::Event { context, value },
            Some(owner),
            Some(lease),
            Conversion::Portable(false),
        )
    }
    fn enqueue_arguments(
        self: &Rc<Self>,
        callback: Rc<Callback>,
        args: InvocationArguments,
        owner: Option<Rc<Owner>>,
        lease: Option<InvocationLease>,
        conversion: Conversion,
    ) -> LocalBoxFuture<'static, Result<Output>> {
        let admitted: Result<_> = (|| {
            self.admit_queue()?;
            let (reply, receive) = oneshot::channel();
            let job = Invocation {
                callback,
                args,
                owner,
                lease,
                conversion,
                reply,
            };
            if self.sender.unbounded_send(Request::Invoke(job)).is_err() {
                self.dequeued();
                return Err(closed());
            }
            Ok(receive)
        })();
        async move { admitted?.await.map_err(|_| closed())? }.boxed_local()
    }
    pub(crate) fn send_owned(&self, request: Request) -> std::result::Result<(), Box<Request>> {
        self.sender
            .unbounded_send(request)
            .map_err(|error| Box::new(error.into_inner()))
    }
    #[cfg(test)]
    pub(crate) fn close_request_channel(&self) {
        self.sender.close_channel();
    }
    pub(crate) fn resolve(&self, request: Request) -> Result<()> {
        self.sender.unbounded_send(request).map_err(|_| closed())
    }
    pub(crate) fn admit_physical(&self) -> Result<()> {
        if self.counters.get().physical_jobs >= self.options.max_physical_jobs {
            return Err(limit("physical_jobs"));
        }
        self.update(|c| c.physical_jobs += 1);
        Ok(())
    }
    pub(crate) fn with_scope<R: 'static>(
        &self,
        body: impl for<'s, 'i> FnOnce(&mut v8::PinScope<'s, 'i>) -> Result<R>,
    ) -> Result<R> {
        if crate::scope::is_active() {
            return crate::scope::evaluate(self.id, body)
                .ok_or_else(|| invalid("engine_scope", "missing scope"))?;
        }
        let mut runtime = self.runtime.try_borrow_mut().map_err(|_| {
            invalid(
                "engine_busy",
                "callback requires active scope or idle engine",
            )
        })?;
        deno_core::scope!(scope, runtime);
        body(scope)
    }
    pub(crate) fn direct(
        &self,
        callback: &Callback,
        args: Vec<Value>,
        kind: crate::scope::CallKind,
    ) -> Result<Value> {
        if let Some(result) = crate::scope::current(self, &callback.function, args.clone(), kind) {
            return result;
        }
        let mut runtime = self.runtime.try_borrow_mut().map_err(|_| {
            invalid(
                "engine_busy",
                "synchronous callback requires current engine scope",
            )
        })?;
        deno_core::scope!(scope, runtime);
        crate::scope::call(scope, &callback.function, args, kind, Some(self))
    }
    fn begin_prepare(
        self: &Rc<Self>,
        host: u64,
        artifact: &BundledJsArtifact,
        export: &str,
    ) -> Result<(usize, Evaluation)> {
        if host == 0 || !artifact.exports.contains(export) {
            return Err(invalid("esm_export", "selected export is undeclared"));
        }
        if artifact.source.len() > self.options.max_source_bytes {
            return Err(limit("source_bytes"));
        }
        let mut runtime = self
            .runtime
            .try_borrow_mut()
            .map_err(|_| invalid("engine_busy", "engine busy"))?;
        {
            deno_core::scope!(scope, runtime);
            let parsed = crate::scope::call(
                scope,
                &self.lexer,
                vec![Value::String(artifact.source.clone())],
                crate::scope::CallKind::Value,
                None,
            )?;
            if parsed[0]
                .as_array()
                .is_none_or(|imports| !imports.is_empty())
            {
                return Err(invalid(
                    "esm_import",
                    "artifact must be import-free without import.meta",
                ));
            }
            let actual: BTreeSet<String> = parsed[1]
                .as_array()
                .ok_or_else(|| invalid("esm_profile", "invalid parser result"))?
                .iter()
                .filter_map(|export| export["n"].as_str().map(str::to_owned))
                .collect();
            if actual.contains("then") || actual != artifact.exports {
                return Err(invalid("esm_exports", "namespace then or export mismatch"));
            }
        }
        let counters = self.counters.get();
        if counters.evaluations >= self.options.max_evaluations
            || artifact.source.len()
                > self
                    .options
                    .max_total_source_bytes
                    .saturating_sub(counters.source_bytes)
        {
            return Err(limit("rotation_required"));
        }
        self.update(|c| {
            c.evaluations += 1;
            c.source_bytes += artifact.source.len();
        });
        let specifier = ModuleSpecifier::parse(&format!(
            "tembra:artifact/{}/{}",
            self.id,
            counters.evaluations + 1
        ))
        .map_err(|e| invalid("esm_artifact", e.to_string()))?;
        let module = runtime
            .load_side_es_module_from_code(&specifier, artifact.source.clone())
            .now_or_never()
            .ok_or_else(|| {
                invalid(
                    "module_loading",
                    "closed module loading unexpectedly yielded",
                )
            })?
            .map_err(js_error)?;
        let evaluation = runtime.mod_evaluate(module).boxed_local();
        Ok((module, evaluation))
    }
}

pub(crate) fn invalid(reason: impl Into<String>, message: impl Into<String>) -> RuntimeError {
    RuntimeError::invalid(reason, message)
}
pub(crate) fn limit(reason: &str) -> RuntimeError {
    RuntimeError::unavailable(reason, "JavaScript executor admission limit reached")
}
pub(crate) fn closed() -> RuntimeError {
    RuntimeError::unavailable("executor_closed", "executor pump ended")
}
fn js_error(error: impl std::fmt::Display) -> RuntimeError {
    invalid("javascript", error.to_string())
}

fn private_lexer(runtime: &mut JsRuntime, source: &str) -> Result<v8::Global<v8::Value>> {
    deno_core::scope!(scope, runtime);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    v8::tc_scope!(let scope, scope);
    let source = v8::String::new(scope, &format!("(()=>{{{source};return parse;}})()"))
        .ok_or_else(|| limit("lexer_source"))?;
    let value = v8::Script::compile(scope, source, None)
        .and_then(|script| script.run(scope))
        .ok_or_else(|| {
            scope.exception().map_or_else(
                || invalid("bootstrap", "lexical parser initialization failed"),
                |exception| {
                    promise_error(deno_core::error::JsError::from_v8_exception(
                        scope, exception,
                    ))
                },
            )
        })?;
    Ok(v8::Global::new(scope, value))
}

pub(crate) fn promise_error(error: Box<deno_core::error::JsError>) -> RuntimeError {
    for (key, value) in &error.additional_properties {
        if key == "tembraRuntimeError"
            && let Ok(error) = serde_json::from_str::<RuntimeError>(value)
        {
            return error;
        }
    }
    js_error(error)
}

fn extensions() -> Vec<deno_core::Extension> {
    let extensions = vec![binding::executor_ops::init()];
    #[cfg(test)]
    let extensions = {
        let mut extensions = extensions;
        extensions.push(tests::test_ops::init());
        extensions
    };
    extensions
}
