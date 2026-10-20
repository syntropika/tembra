//! JavaScript callbacks are execution adapters over the existing Rust Host.

mod activity;
mod adapters;
mod callable;
mod context;
mod factory;
mod handles;
mod hooks;
mod native;
mod reload;
mod scope;
mod support;

pub use activity::{WasmEventSink, WasmTaskScope};
pub use adapters::WasmImplementationView;
pub use context::WasmManagedContext;
pub use context::WasmSignal;
pub use factory::{WasmObjectRecorder, WasmResourceBuilder};
pub use handles::WasmCapability;
pub use handles::{WasmBoundReference, WasmCell, WasmResource, WasmState};
pub use native::{
    BrowserMethodCodec, BrowserMethodHandler, BrowserMethodWrapper, BrowserNativeSignatures,
    BrowserValue,
};
pub use reload::{WasmFacetCandidate, WasmReloadBuilder};
pub use scope::{WasmFacetRecorder, WasmFacetScope};

use crate::{WasmHost, boundary::failure, runtime::WebHost};
use futures::{FutureExt, channel::oneshot, future::LocalBoxFuture};
use js_sys::{Array, Function, Promise};
use serde_json::json;
use std::{
    cell::Cell,
    rc::{Rc, Weak},
};
use support::*;
use tembra_core::{
    Context, ExecutableFacet, InvocationLease, InvocationOwner, LoadOptions, Result, RuntimeError,
};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::{JsFuture, future_to_promise};

pub(crate) struct Session {
    js_opaque: std::cell::RefCell<std::collections::BTreeSet<String>>,
    hooks: std::cell::RefCell<
        std::collections::BTreeMap<String, tembra_hooks::portable::PortableHook>,
    >,
    activities: activity::Cache,
    token: JsValue,
    pub host: Weak<WebHost>,
    pub native: Rc<BrowserNativeSignatures>,
    generations: Cell<u64>,
    artifact_evaluations: Cell<u64>,
    source_bytes: Cell<u64>,
    live_generations: Cell<u64>,
    callbacks: Cell<u64>,
    next_slot: Cell<u64>,
    jobs: Cell<u64>,
    queued: Cell<u64>,
    factories: Cell<u64>,
    callables: std::cell::RefCell<std::collections::BTreeMap<u64, Rc<callable::Slot>>>,
}
pub(crate) struct Generation {
    session: Rc<Session>,
}
impl Drop for Generation {
    fn drop(&mut self) {
        self.session
            .live_generations
            .set(self.session.live_generations.get() - 1);
    }
}
pub(crate) struct Callback {
    generation: Rc<Generation>,
    function: Function,
    _slot: u64,
}
impl Drop for Callback {
    fn drop(&mut self) {
        self.generation
            .session
            .callbacks
            .set(self.generation.session.callbacks.get() - 1);
    }
}
impl Generation {
    pub(crate) fn callback(self: &Rc<Self>, function: Function) -> Result<Rc<Callback>> {
        let session = &self.session;
        if session.callbacks.get() >= 1024 {
            return Err(RuntimeError::unavailable(
                "callbacks_limit",
                "callback capacity exhausted",
            ));
        }
        let slot = session
            .next_slot
            .get()
            .checked_add(1)
            .filter(|n| *n <= 9_007_199_254_740_991)
            .ok_or_else(|| {
                RuntimeError::unavailable("callbacks_limit", "callback identity exhausted")
            })?;
        session.next_slot.set(slot);
        session.callbacks.set(session.callbacks.get() + 1);
        Ok(Rc::new(Callback {
            generation: self.clone(),
            function,
            _slot: slot,
        }))
    }
    pub(crate) fn host(&self) -> Result<Rc<WebHost>> {
        self.session
            .host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "browser Host ended"))
    }
}
impl Callback {
    pub(crate) fn call(&self, args: &Array) -> Result<JsValue> {
        self.generation.host()?;
        self.function
            .apply(&JsValue::UNDEFINED, args)
            .map_err(js_error)
    }
    pub(crate) fn sync(&self, args: &Array) -> Result<JsValue> {
        synchronous(self.call(args)?)
    }

    /// The spawned observer, not the reply waiter, owns physical and generation retention.
    pub(crate) fn invoke(
        self: &Rc<Self>,
        context: Option<Context>,
        args: Array,
        lease: Option<InvocationLease>,
        invocation: Option<InvocationOwner>,
    ) -> LocalBoxFuture<'static, Result<JsValue>> {
        let callback = self.clone();
        async move {
            let session = callback.generation.session.clone();
            if session.jobs.get() >= 128 || session.queued.get() >= 64 {
                return Err(RuntimeError::unavailable(
                    "jobs_limit",
                    "browser callback capacity exhausted",
                ));
            }
            session.jobs.set(session.jobs.get() + 1);
            session.queued.set(session.queued.get() + 1);
            let (send, receive) = oneshot::channel();
            wasm_bindgen_futures::spawn_local(async move {
                let _job = Job {
                    session: session.clone(),
                    _physical: lease,
                };
                session.queued.set(session.queued.get() - 1);
                let result = async {
                    let _context = if let Some(context) = context.as_ref() {
                        context.check(callback.generation.host()?.host.id())?;
                        let context = context::WasmManagedContext::issue(
                            &callback.generation,
                            context.clone(),
                            invocation,
                        )?;
                        args.unshift(&JsValue::from(context.clone_wrapper()));
                        Some(context)
                    } else {
                        None
                    };
                    let value = callback.call(&args)?;
                    let value = JsFuture::from(observe_owned(&value).map_err(js_error)?)
                        .await
                        .map_err(js_error)?;
                    // Core cancellation is cooperative. A callback that was admitted with
                    // a live Context may return a late value; later Host calls still check
                    // that Context, and the physical lease lasts through conversion.
                    Ok(value)
                }
                .await;
                let _ = send.send(result);
            });
            receive.await.map_err(|_| {
                RuntimeError::unavailable("javascript", "callback observation ended")
            })?
        }
        .boxed_local()
    }
}
struct Job {
    session: Rc<Session>,
    _physical: Option<InvocationLease>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.session.jobs.set(self.session.jobs.get() - 1);
    }
}

/// Browser execution-session admission. It holds no strong Host or object owner.
#[wasm_bindgen]
pub struct WasmExecutor {
    pub(crate) session: Rc<Session>,
    application: Rc<Generation>,
}

/// Binds a JavaScript executor to the actual existing Host, never a replacement Host.
#[wasm_bindgen(js_name = createCheckedWasmExecutor)]
pub fn create_checked_wasm_executor(host: &WasmHost) -> WasmExecutor {
    let session = Rc::new(Session {
        activities: activity::Cache::default(),
        hooks: std::cell::RefCell::default(),
        js_opaque: std::cell::RefCell::default(),
        token: crate::intrinsics::fresh_context_token(),
        host: Rc::downgrade(&host.inner),
        native: Rc::new(BrowserNativeSignatures::default()),
        generations: Cell::new(0),
        artifact_evaluations: Cell::new(0),
        source_bytes: Cell::new(0),
        live_generations: Cell::new(1),
        callbacks: Cell::new(0),
        next_slot: Cell::new(0),
        jobs: Cell::new(0),
        queued: Cell::new(0),
        factories: Cell::new(0),
        callables: std::cell::RefCell::default(),
    });
    WasmExecutor {
        application: Rc::new(Generation {
            session: session.clone(),
        }),
        session,
    }
}
impl WasmExecutor {
    /// Allocates a callback owner without claiming that JavaScript was evaluated here.
    /// Closed-artifact admission and native-module admission are separate loader policies.
    pub(super) fn callback_generation(&self) -> Result<WasmGeneration> {
        self.host_id()?;
        if self.session.live_generations.get() >= 64 {
            return Err(RuntimeError::unavailable(
                "generations_limit",
                "live callback generation capacity exhausted",
            ));
        }
        let id = self
            .session
            .generations
            .get()
            .checked_add(1)
            .filter(|id| *id <= 9_007_199_254_740_991)
            .ok_or_else(|| {
                RuntimeError::unavailable("generations_limit", "generation identity exhausted")
            })?;
        self.session.generations.set(id);
        self.session
            .live_generations
            .set(self.session.live_generations.get() + 1);
        Ok(WasmGeneration {
            inner: Rc::new(Generation {
                session: self.session.clone(),
            }),
        })
    }
    /// Gives embedding code the checked whole-signature adapter registry.
    pub fn native_adapters(&self) -> Rc<BrowserNativeSignatures> {
        self.session.native.clone()
    }
    /// Observes Host provenance without granting construction authority.
    /// # Errors
    /// Refuses a dropped browser Host; its numeric observation grants no authority.
    pub fn host_id(&self) -> Result<u64> {
        self.session
            .host
            .upgrade()
            .map(|h| h.host.id())
            .ok_or_else(|| RuntimeError::unavailable("closed", "browser Host ended"))
    }
}
#[wasm_bindgen]
impl WasmExecutor {
    /// Registers generic adapters for a portable hook before contribution installation.
    /// # Errors
    /// Refuses invalid descriptors, native payloads and adapter conflicts/capacity.
    #[wasm_bindgen(js_name = registerHook)]
    pub fn register_hook(&self, descriptor: JsValue) -> std::result::Result<(), JsValue> {
        let descriptor =
            tembra_hooks::HookDescriptor::from_json(&string(&descriptor).map_err(failure)?)
                .map_err(failure)?;
        self.register_portable_hook(descriptor).map_err(failure)
    }
    /// Explicitly enables resource-backed realm-local JavaScript opaque values.
    /// # Errors
    /// Refuses invalid nominals, conflicts with Rust adapters and exhausted capacity.
    #[wasm_bindgen(js_name = registerOpaque)]
    pub fn register_opaque(&self, nominal: JsValue) -> std::result::Result<(), JsValue> {
        let nominal = string(&nominal).map_err(failure)?;
        if self.session.js_opaque.borrow().contains(&nominal) {
            return Ok(());
        }
        self.session
            .native
            .register_opaque::<BrowserValue>(
                &nominal,
                |value| Ok(value.0.clone()),
                |value| Ok(Rc::new(BrowserValue(value))),
            )
            .map_err(failure)?;
        self.session.js_opaque.borrow_mut().insert(nominal);
        Ok(())
    }
    /// Acquires a consumer through this executor's actual Host and shared reference adapter.
    /// # Errors
    /// Refuses invalid descriptors, input/options and closed core admission before acquisition.
    pub fn hold(
        &self,
        descriptor: JsValue,
        input: JsValue,
        options: JsValue,
    ) -> std::result::Result<WasmBoundReference, JsValue> {
        let host = self.application.host().map_err(failure)?;
        let options = strict_json(&options).map_err(failure)?;
        let fields = options
            .as_object()
            .ok_or_else(|| failure(error("options", "expected acquisition options")))?;
        if fields.keys().any(|key| key != "state") {
            return Err(failure(error("options", "unknown acquisition option")));
        }
        let state = match fields.get("state") {
            None => true,
            Some(serde_json::Value::Bool(v)) => *v,
            _ => return Err(failure(error("options", "state must be boolean"))),
        };
        let reference = host
            .host
            .hold(
                contract(&descriptor).map_err(failure)?,
                strict_json(&input).map_err(failure)?,
                tembra_core::GetOptions {
                    state,
                    ..Default::default()
                },
            )
            .map_err(failure)?;
        Ok(WasmBoundReference::new(reference, &self.application))
    }

    /// Runs an application root through the same managed callback adapter as installed facets.
    /// # Errors
    /// Refuses closed admission and propagates callback errors after any completed effects.
    #[wasm_bindgen(js_name = runRoot)]
    pub fn run_root(&self, callback: Function) -> std::result::Result<Promise, JsValue> {
        let generation = self.application.clone();
        let host = generation.host().map_err(failure)?.host.clone();
        let callback = generation.callback(callback).map_err(failure)?;
        Ok(future_to_promise(async move {
            host.root(move |context| callback.invoke(Some(context), Array::new(), None, None))
                .await
                .map_err(failure)
        }))
    }

    /// Returns the core-owned registry view for this Host.
    /// # Errors
    /// Refuses malformed definitions, conflicting policy or a closed Host.
    pub fn registry(
        &self,
        definition: JsValue,
    ) -> std::result::Result<scope::WasmRegistryView, JsValue> {
        scope::registry_view(&self.application, &definition).map_err(failure)
    }

    /// Reserves a lifetime evaluation budget before executing source, including failed evaluations.
    /// # Errors
    /// Refuses invalid size, exhausted lifetime evaluation/source budget or an unavailable Host
    /// before executing source.
    #[wasm_bindgen(js_name = reserveGeneration)]
    pub fn reserve_generation(&self, bytes: f64) -> std::result::Result<WasmGeneration, JsValue> {
        if !bytes.is_finite() || bytes.fract() != 0.0 || !(0.0..=1_048_576.0).contains(&bytes) {
            return Err(failure(error(
                "artifact_size",
                "artifact source exceeds 1 MiB",
            )));
        }
        let bytes = bytes as u64;
        if self.session.artifact_evaluations.get() >= 32
            || self.session.source_bytes.get() + bytes > 8_388_608
        {
            return Err(failure(RuntimeError::unavailable(
                "rotation_required",
                "execution session evaluation budget exhausted",
            )));
        }
        let generation = self.callback_generation().map_err(failure)?;
        self.session
            .artifact_evaluations
            .set(self.session.artifact_evaluations.get() + 1);
        self.session
            .source_bytes
            .set(self.session.source_bytes.get() + bytes);
        Ok(generation)
    }
    /// Installs an explicitly preloaded trusted facet. Closed source loading uses a reserved
    /// generation.
    /// # Errors
    /// Refuses invalid metadata/callback capacity, unavailable Host or synchronous setup/service
    /// admission. The readiness Promise can reject after setup effects; dropping it does not
    /// withdraw services.
    pub fn install(
        &self,
        name: JsValue,
        uses: JsValue,
        setup: Function,
    ) -> std::result::Result<Promise, JsValue> {
        self.callback_generation()
            .map_err(failure)?
            .install(name, uses, setup)
    }
    #[wasm_bindgen(js_name = diagnosticsJson)]
    pub fn diagnostics_json(&self) -> String {
        json!({
            "generations": self.session.generations.get(),
            "artifactEvaluations": self.session.artifact_evaluations.get(),
            "liveGenerations": self.session.live_generations.get(),
            "sourceBytes": self.session.source_bytes.get(),
            "callbacks": self.session.callbacks.get(),
            "jobs": self.session.jobs.get(),
            "queued": self.session.queued.get(),
        })
        .to_string()
    }
}

/// A budgeted module generation; dropping it does not evict browser ESM cache entries.
#[wasm_bindgen]
pub struct WasmGeneration {
    inner: Rc<Generation>,
}
#[wasm_bindgen]
impl WasmGeneration {
    /// # Errors
    /// Refuses invalid metadata/callback capacity, unavailable Host or synchronous setup/service
    /// admission. The readiness Promise can reject after setup effects; dropping it does not
    /// withdraw services.
    pub fn install(
        &self,
        name: JsValue,
        uses: JsValue,
        setup: Function,
    ) -> std::result::Result<Promise, JsValue> {
        let generation = self.inner.clone();
        let host = generation.host().map_err(failure)?;
        let name = string(&name).map_err(failure)?;
        let values = strict_json(&uses).map_err(failure)?;
        let uses = values
            .as_array()
            .ok_or_else(|| failure(error("facet_uses", "expected descriptor array")))?
            .iter()
            .map(|v| contract(&JsValue::from_str(&v.to_string())))
            .collect::<Result<Vec<_>>>()
            .map_err(failure)?;
        let setup = generation.callback(setup).map_err(failure)?;
        let expected = host.host.id();
        let facet = ExecutableFacet::foreign(name, uses, move |scope, declaration| {
            if scope.host_id() != expected {
                return Err(error("foreign_host", "facet belongs to another Host"));
            }
            let args = Array::new();
            args.push(&scope::WasmFacetScope::new(scope.clone(), &generation).into());
            args.push(&scope::WasmFacetRecorder::new(declaration.recorder(), &generation).into());
            let result = setup.sync(&args)?;
            if !result.is_undefined() {
                return Err(error("facet_setup", "setup must return undefined"));
            }
            Ok(())
        })
        .map_err(failure)?;
        let startup = host
            .host
            .install_executable(facet, LoadOptions::default())
            .map_err(failure)?;
        Ok(future_to_promise(async move {
            startup.await.map_err(failure)?;
            Ok(JsValue::UNDEFINED)
        }))
    }
}
