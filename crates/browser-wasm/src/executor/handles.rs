use super::{
    Generation,
    context::{call_context, checked_context},
    native::BrowserValue,
    support::*,
};
use crate::{
    WasmSubscription,
    boundary::failure,
    readable::snapshot,
    runtime::{WebHost, shape, status},
};
use js_sys::{Array, Function, Promise};
use serde_json::json;
use std::rc::{Rc, Weak};
use tembra_core::contracts::Member;
use tembra_core::{Cell, Readable, Reference, ReplaceOptions, Result, RuntimeError, WeakResource};
use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::future_to_promise;

pub(crate) fn generation(weak: &Weak<Generation>) -> Result<Rc<Generation>> {
    weak.upgrade()
        .ok_or_else(|| RuntimeError::unavailable("generation", "execution generation ended"))
}

/// A checked weak source into the core graph, without a JavaScript state mirror.
#[wasm_bindgen]
pub struct WasmState {
    pub(crate) source: Readable,
    pub(crate) host: Weak<WebHost>,
    pub(crate) generation: Weak<Generation>,
}
impl WasmState {
    pub(crate) fn new(source: Readable, generation: &Rc<Generation>) -> Self {
        Self {
            source,
            host: generation.session.host.clone(),
            generation: Rc::downgrade(generation),
        }
    }
    pub(crate) fn check_host(&self, expected: &Weak<WebHost>) -> Result<()> {
        if !Weak::ptr_eq(&self.host, expected) {
            return Err(error("foreign_host", "state belongs to another Host"));
        }
        Ok(())
    }
}
#[wasm_bindgen]
impl WasmState {
    /// # Errors
    /// Refuses uninitialized, ended or disallowed sources through the actual core readable.
    #[wasm_bindgen(js_name = snapshotJson)]
    pub fn snapshot_json(&self) -> std::result::Result<String, JsValue> {
        Ok(snapshot(&self.source.snapshot().map_err(failure)?).to_string())
    }
    /// Reads an initializer's private candidate view only through its actual checked Context.
    ///
    /// # Errors
    /// Refuses foreign/expired Contexts and the core's candidate read-authority failures.
    #[wasm_bindgen(js_name = snapshotInJson)]
    pub fn snapshot_in_json(&self, token: JsValue) -> std::result::Result<String, JsValue> {
        let context = checked_context(&self.host, &self.generation, &token).map_err(failure)?;
        Ok(snapshot(&self.source.snapshot_in(&context).map_err(failure)?).to_string())
    }
    /// # Errors
    /// Refuses unavailable generation or core subscription admission. Observer errors follow
    /// committed effects.
    pub fn subscribe(&self, callback: Function) -> std::result::Result<WasmSubscription, JsValue> {
        let callback = generation(&self.generation)
            .map_err(failure)?
            .callback(callback)
            .map_err(failure)?;
        let subscription = self
            .source
            .subscribe(move |value| {
                let args = Array::new();
                args.push(&JsValue::from_str(&snapshot(&value).to_string()));
                callback.sync(&args).map(|_| ())
            })
            .map_err(failure)?;
        Ok(WasmSubscription::new(move || subscription.unsubscribe()))
    }
}

/// A weak writable capability. Context validation and candidate adoption remain in Rust.
#[wasm_bindgen]
pub struct WasmCell {
    pub(crate) cell: Cell,
    pub(crate) host: Weak<WebHost>,
    pub(crate) generation: Weak<Generation>,
}
#[wasm_bindgen]
impl WasmCell {
    /// # Errors
    /// Refuses invalid/unavailable state members and unavailable generation; the returned source is
    /// weak.
    pub fn state(&self) -> WasmState {
        WasmState {
            source: self.cell.readable(),
            host: self.host.clone(),
            generation: self.generation.clone(),
        }
    }
    /// # Errors
    /// Refuses invalid Context/value or core preparation/adoption; invalid candidates leave prior
    /// state unchanged.
    pub fn replace(
        &self,
        token: JsValue,
        value: JsValue,
        diff: bool,
    ) -> std::result::Result<String, JsValue> {
        let context = checked_context(&self.host, &self.generation, &token).map_err(failure)?;
        Ok(snapshot(
            &self
                .cell
                .replace(
                    &context,
                    strict_json(&value).map_err(failure)?,
                    ReplaceOptions { diff },
                )
                .map_err(failure)?,
        )
        .to_string())
    }
    /// # Errors
    /// Refuses invalid Context, asynchronous callback, invalid candidate or core read-set conflict
    /// before adoption; callback external effects are not rolled back.
    pub fn change(&self, token: JsValue, mutate: Function) -> std::result::Result<String, JsValue> {
        let context = checked_context(&self.host, &self.generation, &token).map_err(failure)?;
        let callback = generation(&self.generation)
            .map_err(failure)?
            .callback(mutate)
            .map_err(failure)?;
        let result = self
            .cell
            .change(&context, |value| {
                let args = Array::new();
                args.push(&JsValue::from_str(&value.to_string()));
                let candidate = strict_json(&callback.sync(&args)?)?;
                *value = candidate;
                Ok(())
            })
            .map_err(failure)?;
        Ok(snapshot(&result).to_string())
    }
}

/// Weak resource observation; the core, not this wrapper, retains its managed value.
#[wasm_bindgen]
pub struct WasmResource {
    pub(crate) resource: WeakResource<BrowserValue>,
    pub(crate) host: Weak<WebHost>,
    pub(crate) generation: Weak<Generation>,
}
#[wasm_bindgen]
impl WasmResource {
    /// # Errors
    /// Refuses an ended resource, factory-time access or a value that is not open. Saved JS aliases
    /// obey the resource usability contract.
    pub fn value(&self) -> std::result::Result<JsValue, JsValue> {
        Ok(self
            .resource
            .upgrade()
            .map_err(failure)?
            .value()
            .map_err(failure)?
            .0
            .clone())
    }
    /// # Errors
    /// Refuses a resource record that no longer exists; observation does not retain it.
    pub fn version(&self) -> std::result::Result<f64, JsValue> {
        Ok(self.resource.upgrade().map_err(failure)?.version() as f64)
    }
    /// # Errors
    /// Refuses a resource record that no longer exists; observation does not retain it.
    pub fn exclusive(&self) -> std::result::Result<bool, JsValue> {
        Ok(self.resource.upgrade().map_err(failure)?.exclusive())
    }
    /// # Errors
    /// Refuses invalid text, unavailable generation or an unmapped nominal adapter. It never reads
    /// the resource value.
    pub fn capability(&self, nominal: JsValue) -> std::result::Result<WasmCapability, JsValue> {
        let nominal = string(&nominal).map_err(failure)?;
        generation(&self.generation)
            .map_err(failure)?
            .session
            .native
            .opaque(&nominal)
            .map_err(failure)?;
        Ok(WasmCapability {
            resource: self.resource.clone(),
            host: self.host.clone(),
            nominal,
        })
    }
}
/// Resource-backed nominal capability; construction is restricted to actual managed handles.
#[wasm_bindgen]
pub struct WasmCapability {
    pub(crate) resource: WeakResource<BrowserValue>,
    pub(crate) host: Weak<WebHost>,
    pub(crate) nominal: String,
}

/// A reference with weak browser-owner provenance, including object-owned dependency handles.
#[wasm_bindgen]
pub struct WasmBoundReference {
    pub(crate) reference: Reference,
    pub(crate) host: Weak<WebHost>,
    generation: Weak<Generation>,
}
impl WasmBoundReference {
    pub(crate) fn new(reference: Reference, generation: &Rc<Generation>) -> Self {
        Self {
            reference,
            host: generation.session.host.clone(),
            generation: Rc::downgrade(generation),
        }
    }
    fn check_call(&self) -> Result<()> {
        if generation(&self.generation)?.session.factories.get() != 0 {
            return Err(RuntimeError::new(
                tembra_core::ErrorCode::NotReady,
                "factory",
                "reference invocation is refused during factories",
            ));
        }
        self.host
            .upgrade()
            .ok_or_else(|| RuntimeError::unavailable("closed", "browser Host ended"))?;
        Ok(())
    }
}
#[wasm_bindgen]
impl WasmBoundReference {
    #[wasm_bindgen(js_name = statusJson)]
    pub fn status_json(&self) -> String {
        json!(status(self.reference.status())).to_string()
    }
    #[wasm_bindgen(js_name = shapeJson)]
    pub fn shape_json(&self) -> String {
        match self.reference.shape() {
            Some(v) => json!({"present":true,"value":shape(v)}),
            None => json!({"present":false}),
        }
        .to_string()
    }
    #[wasm_bindgen(js_name = errorJson)]
    pub fn error_json(&self) -> String {
        match self.reference.error() {
            Some(v) => json!({"present":true,"value":v}),
            None => json!({"present":false}),
        }
        .to_string()
    }
    /// # Errors
    /// Refuses factory-time invocation or an unavailable owner; the Promise reports ordinary
    /// reference readiness failures.
    pub fn ready(&self) -> std::result::Result<Promise, JsValue> {
        self.check_call().map_err(failure)?;
        let reference = self.reference.clone();
        Ok(future_to_promise(async move {
            reference.ready().await.map_err(failure)?;
            Ok(JsValue::UNDEFINED)
        }))
    }
    /// # Errors
    /// Refuses factory-time access, malformed payload/Context or an unmapped native signature.
    /// Native signature calls require an explicit managed Context before any codec runs.
    /// Admitted effects are not rolled back by later cancellation/error.
    pub fn call(
        &self,
        member: JsValue,
        input: JsValue,
        token: JsValue,
        native: bool,
    ) -> std::result::Result<Promise, JsValue> {
        self.check_call().map_err(failure)?;
        if native && token.is_undefined() {
            return Err(failure(error(
                "native_context",
                "native signature calls require an explicit managed Context",
            )));
        }
        let member = string(&member).map_err(failure)?;
        let (context, physical) = if token.is_undefined() {
            (None, None)
        } else {
            let (context, physical) =
                call_context(&self.host, &self.generation, &token).map_err(failure)?;
            (Some(context), physical)
        };
        let reference = self.reference.clone();
        let generation = generation(&self.generation).map_err(failure)?;
        let payload = if native {
            None
        } else {
            Some(payload(strict_json(&input).map_err(failure)?).map_err(failure)?)
        };
        Ok(future_to_promise(async move {
            // The originating JS callback may finish before this detached downstream
            // call. Its physical invocation remains pinned through result conversion.
            let _physical = physical;
            if native {
                // Nominal signatures are local-only. Delegate readiness and its Context
                // cancellation race to Reference::call instead of awaiting a separate fence.
                let schema = reference
                    .contract()
                    .members("")
                    .and_then(|v| v.get(&member))
                    .ok_or_else(|| failure(error("method", "method is not in selected variant")))?;
                let adapter = generation.session.native.method(schema).map_err(failure)?;
                adapter
                    .call(reference, member, input, context)
                    .await
                    .map_err(failure)
            } else {
                let result = reference
                    .call_json(&member, payload.flatten(), context.as_ref())
                    .await
                    .map_err(failure)?;
                Ok(JsValue::from_str(&envelope(result).to_string()))
            }
        }))
    }
    /// # Errors
    /// Refuses invalid/unavailable member, nominal adapter or core value/type admission before
    /// conversion.
    pub fn opaque(&self, member: JsValue) -> std::result::Result<JsValue, JsValue> {
        let member = string(&member).map_err(failure)?;
        let Some(Member::Opaque { id }) = self
            .reference
            .contract()
            .members("")
            .and_then(|v| v.get(&member))
        else {
            return Err(failure(error(
                "opaque",
                "member is not an opaque capability",
            )));
        };
        generation(&self.generation)
            .map_err(failure)?
            .session
            .native
            .opaque(id)
            .map_err(failure)?
            .read(&self.reference, &member)
            .map_err(failure)
    }
    pub fn release(&self) {
        self.reference.release();
    }
    /// # Errors
    /// Refuses invalid/unavailable state members and unavailable generation; the returned source is
    /// weak.
    pub fn state(&self, member: JsValue) -> std::result::Result<WasmState, JsValue> {
        Ok(WasmState::new(
            self.reference
                .readable(&string(&member).map_err(failure)?)
                .map_err(failure)?,
            &generation(&self.generation).map_err(failure)?,
        ))
    }
    /// # Errors
    /// Refuses unavailable generation/callback capacity. Callback errors are reported separately
    /// from committed lifecycle effects.
    #[wasm_bindgen(js_name = onStatus)]
    pub fn on_status(&self, listener: Function) -> std::result::Result<WasmSubscription, JsValue> {
        let callback = generation(&self.generation)
            .map_err(failure)?
            .callback(listener)
            .map_err(failure)?;
        let host = self.host.clone();
        let stop = self.reference.on_status(move |value, error_value| {
            let args = Array::new();
            args.push(&JsValue::from_str(
                &json!({"status":status(value),"error":error_value}).to_string(),
            ));
            if let Err(error_value) = callback.sync(&args)
                && let Some(host) = host.upgrade()
            {
                host.report(error_value);
            }
        });
        Ok(WasmSubscription::new(stop))
    }
    /// # Errors
    /// Refuses unavailable generation/callback capacity. Callback errors do not undo the observed
    /// shape.
    #[wasm_bindgen(js_name = onShape)]
    pub fn on_shape(&self, listener: Function) -> std::result::Result<WasmSubscription, JsValue> {
        let callback = generation(&self.generation)
            .map_err(failure)?
            .callback(listener)
            .map_err(failure)?;
        let host = self.host.clone();
        let stop = self.reference.on_shape(move |value| {
            let args = Array::new();
            args.push(&JsValue::from_str(&shape(value).to_string()));
            if let Err(error_value) = callback.sync(&args)
                && let Some(host) = host.upgrade()
            {
                host.report(error_value);
            }
        });
        Ok(WasmSubscription::new(stop))
    }
}
