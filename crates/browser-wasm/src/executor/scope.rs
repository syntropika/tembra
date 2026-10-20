use super::{
    Generation,
    context::{WasmSignal, call_context, checked_context},
    factory::run_factory,
    handles::generation,
    support::*,
};
use crate::{WasmSubscription, boundary::failure, runtime::WebHost};
use futures::FutureExt;
use js_sys::{Array, Function, Promise};
use serde_json::{Value, json};
use std::{
    rc::{Rc, Weak},
    time::Duration,
};
use tembra_core::{
    ConflictPolicy, FacetRecorder, FacetScope, Lifetime, RegisteredOperation, Registry,
    RegistrySnapshot, RegistryView, Result, ServeOptions,
};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::future_to_promise;

pub(crate) fn registry(value: &JsValue) -> Result<Registry> {
    let value = strict_json(value)?;
    let fields = value
        .as_object()
        .ok_or_else(|| error("registry", "expected registry definition"))?;
    if fields.len() != 2 || !fields.contains_key("name") || !fields.contains_key("conflict") {
        return Err(error(
            "registry",
            "registry definition has unknown or missing fields",
        ));
    }
    let name = fields["name"]
        .as_str()
        .ok_or_else(|| error("registry", "registry name must be text"))?;
    let conflict = match fields["conflict"].as_str() {
        Some("error") => ConflictPolicy::Error,
        Some("last") => ConflictPolicy::Last,
        _ => return Err(error("registry", "unknown conflict policy")),
    };
    Registry::new(name, conflict)
}

/// Weak, retainable run API. Setup recorder authority is a separate object.
#[wasm_bindgen]
pub struct WasmFacetScope {
    scope: FacetScope,
    generation: Weak<Generation>,
}
impl WasmFacetScope {
    pub(crate) fn new(scope: FacetScope, generation: &Rc<Generation>) -> Self {
        Self {
            scope,
            generation: Rc::downgrade(generation),
        }
    }
}
#[wasm_bindgen]
impl WasmFacetScope {
    #[wasm_bindgen(js_name = hostName)]
    pub fn host_name(&self) -> String {
        self.scope.host_name().into()
    }
    pub fn signal(&self) -> WasmSignal {
        WasmSignal::facet(self.scope.signal())
    }
    /// # Errors
    /// Refuses unavailable generation, callback capacity or ordinary Host root admission.
    /// Cancellation does not establish physical callback completion.
    pub fn root(&self, callback: Function) -> std::result::Result<Promise, JsValue> {
        let callback = generation(&self.generation)
            .map_err(failure)?
            .callback(callback)
            .map_err(failure)?;
        let future = self.scope.root(move |context| {
            async move {
                callback
                    .invoke(Some(context), Array::new(), None, None)
                    .await
            }
            .boxed_local()
        });
        Ok(future_to_promise(
            async move { future.await.map_err(failure) },
        ))
    }
    /// # Errors
    /// Refuses invalid/conflicting registry metadata or unavailable Host/generation admission.
    pub fn registry(&self, definition: JsValue) -> std::result::Result<WasmRegistryView, JsValue> {
        let generation = generation(&self.generation).map_err(failure)?;
        Ok(WasmRegistryView {
            view: self
                .scope
                .registry(&registry(&definition).map_err(failure)?)
                .map_err(failure)?,
            generation: Rc::downgrade(&generation),
            host: generation.session.host.clone(),
        })
    }
}

/// Weak exact-pass setup declarations; saved instances expire with core setup authority.
#[wasm_bindgen]
pub struct WasmFacetRecorder {
    recorder: FacetRecorder,
    generation: Weak<Generation>,
}
impl WasmFacetRecorder {
    pub(crate) fn new(recorder: FacetRecorder, generation: &Rc<Generation>) -> Self {
        Self {
            recorder,
            generation: Rc::downgrade(generation),
        }
    }
}
#[wasm_bindgen]
impl WasmFacetRecorder {
    /// Reports the separate executable-adapter channel supported by this binding.
    #[wasm_bindgen(js_name = adapterVersion)]
    pub fn adapter_version(&self) -> u32 {
        1
    }

    /// # Errors
    /// Refuses invalid descriptor/options, expired setup pass or core service admission. Earlier
    /// setup effects follow the core withdrawal policy.
    pub fn serve(
        &self,
        descriptor: JsValue,
        factory: Function,
        flags: JsValue,
        adapters: JsValue,
    ) -> std::result::Result<(), JsValue> {
        let contract = contract(&descriptor).map_err(failure)?;
        let generation = generation(&self.generation).map_err(failure)?;
        let callback = generation.callback(factory).map_err(failure)?;
        let mut options = ServeOptions::default();
        let flags = strict_json(&flags).map_err(failure)?;
        let flags = flags
            .as_object()
            .ok_or_else(|| failure(error("serve_options", "expected serve options")))?;
        if flags.keys().any(|key| key != "lifetime" && key != "linger") {
            return Err(failure(error("serve_options", "unknown serve option")));
        }
        options.lifetime = match flags.get("lifetime").and_then(Value::as_str) {
            None if !flags.contains_key("lifetime") => Lifetime::Referenced,
            Some("ref") => Lifetime::Referenced,
            Some("host") => Lifetime::Host,
            _ => return Err(failure(error("serve_options", "unknown lifetime"))),
        };
        if let Some(linger) = flags.get("linger") {
            let ms = linger
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 9_007_199_254_740_991.0)
                .ok_or_else(|| {
                    failure(error(
                        "serve_options",
                        "linger must be nonnegative finite milliseconds",
                    ))
                })?;
            options.linger = Duration::from_secs_f64(ms / 1000.0);
        }
        options.accepts =
            super::adapters::parse(&generation, &contract, &adapters).map_err(failure)?;
        let token = contract.clone();
        self.recorder
            .serve(
                contract,
                Rc::new(move |input, principal, declaration| {
                    run_factory(
                        &generation,
                        &callback,
                        &token,
                        input,
                        principal,
                        declaration.recorder(),
                    )
                }),
                options,
            )
            .map_err(failure)
    }
}

/// Version-selected registry view; the core owns tables and observer ordering.
#[wasm_bindgen]
pub struct WasmRegistryView {
    view: RegistryView,
    generation: Weak<Generation>,
    host: Weak<WebHost>,
}
#[wasm_bindgen]
impl WasmRegistryView {
    /// # Errors
    /// Refuses invalid Context, closed Host or unavailable exact epoch; no current-version
    /// substitution is made.
    pub fn table(&self, token: JsValue) -> std::result::Result<WasmRegistrySnapshot, JsValue> {
        let context = if token.is_undefined() {
            None
        } else {
            Some(checked_context(&self.host, &self.generation, &token).map_err(failure)?)
        };
        Ok(WasmRegistrySnapshot {
            snapshot: self.view.table(context.as_ref()).map_err(failure)?,
            host: self.host.clone(),
            generation: self.generation.clone(),
        })
    }
    /// # Errors
    /// Refuses factory-time subscription, unavailable owner/generation or callback capacity.
    /// Initial observer errors are reported separately.
    pub fn subscribe(&self, listener: Function) -> std::result::Result<WasmSubscription, JsValue> {
        let callback = generation(&self.generation)
            .map_err(failure)?
            .callback(listener)
            .map_err(failure)?;
        let host = self.host.clone();
        let generation = self.generation.clone();
        let subscription = self
            .view
            .subscribe(move |snapshot| {
                let args = Array::new();
                args.push(
                    &WasmRegistrySnapshot {
                        snapshot,
                        host: host.clone(),
                        generation: generation.clone(),
                    }
                    .into(),
                );
                callback.sync(&args)?;
                Ok(())
            })
            .map_err(failure)?;
        Ok(WasmSubscription::new(move || subscription.unsubscribe()))
    }
}
/// Detached immutable table data; operations retain only checked weak admission.
#[wasm_bindgen]
pub struct WasmRegistrySnapshot {
    snapshot: RegistrySnapshot,
    host: Weak<WebHost>,
    generation: Weak<Generation>,
}
#[wasm_bindgen]
impl WasmRegistrySnapshot {
    #[wasm_bindgen(js_name = entriesJson)]
    pub fn entries_json(&self) -> String {
        let entries: Vec<_> = self
            .snapshot
            .entries()
            .map(|(key, entry)| {
                json!({
                    "key": key,
                    "data": entry.data(),
                    "operation": entry.operation().is_some(),
                })
            })
            .collect();
        json!({"codeVersion":self.snapshot.code_version(),"entries":entries}).to_string()
    }
    /// # Errors
    /// Refuses invalid key text; missing keys/non-operation entries return absence without
    /// acquiring a claim.
    pub fn operation(
        &self,
        key: JsValue,
    ) -> std::result::Result<Option<WasmRegisteredOperation>, JsValue> {
        let key = string(&key).map_err(failure)?;
        Ok(self
            .snapshot
            .get(&key)
            .and_then(|entry| entry.operation())
            .cloned()
            .map(|operation| WasmRegisteredOperation {
                operation,
                host: self.host.clone(),
                generation: self.generation.clone(),
            }))
    }
}
/// Calls the existing weak version-bound core operation with explicit Context authority.
#[wasm_bindgen]
pub struct WasmRegisteredOperation {
    operation: RegisteredOperation,
    host: Weak<WebHost>,
    generation: Weak<Generation>,
}
#[wasm_bindgen]
impl WasmRegisteredOperation {
    /// # Errors
    /// Refuses missing method metadata or encoding failure; this observation grants no owner
    /// admission.
    #[wasm_bindgen(js_name = metadataJson)]
    pub fn metadata_json(&self) -> std::result::Result<String, JsValue> {
        let member = self
            .operation
            .contract()
            .members("")
            .and_then(|members| members.get(self.operation.method()))
            .ok_or_else(|| failure(error("method", "registered method metadata is missing")))?;
        serde_json::to_string(member)
            .map_err(|_| failure(error("method", "cannot encode method metadata")))
    }
    /// # Errors
    /// Refuses invalid Context/payload, unavailable generation or native mapping. Core checks
    /// closing, owner and exact epoch again on actual dispatch.
    pub fn call(
        &self,
        token: JsValue,
        input: JsValue,
        native: bool,
    ) -> std::result::Result<Promise, JsValue> {
        let (context, physical) =
            call_context(&self.host, &self.generation, &token).map_err(failure)?;
        let operation = self.operation.clone();
        let generation = generation(&self.generation).map_err(failure)?;
        if native {
            let member = operation
                .contract()
                .members("")
                .and_then(|members| members.get(operation.method()))
                .ok_or_else(|| failure(error("method", "registered method metadata is missing")))?;
            let adapter = generation.session.native.method(member).map_err(failure)?;
            return Ok(future_to_promise(async move {
                let _physical = physical;
                adapter
                    .call_registered(operation, input, context)
                    .await
                    .map_err(failure)
            }));
        }
        let args = payload(strict_json(&input).map_err(failure)?).map_err(failure)?;
        Ok(future_to_promise(async move {
            let _physical = physical;
            Ok(JsValue::from_str(
                &envelope(operation.call_json(&context, args).await.map_err(failure)?).to_string(),
            ))
        }))
    }
}
